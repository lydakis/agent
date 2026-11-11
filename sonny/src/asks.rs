//! Decisions for the person: the lead calls the `ask` tool Sonny declares
//! on it, which the daemon records, and Sonny draws the question in the
//! conversation where it was asked: why it is asked, each option with what
//! picking it means, the one the lead recommends and why, and what the
//! agent does while nobody answers. A pick (a click on an option, or its
//! number typed) answers it, and so do words typed while it is in focus in
//! the composer, the row over them naming it: either goes to the lead as
//! their next message, once. Words written after Esc are the person's own,
//! never framed as an answer, and set the question aside: the agent reads
//! whether they answer it, as a reply under a card in a chat is read, and
//! closes it with [`CLOSE`] once it knows. A card says which of these it
//! is in ([`State`]). The open questions are read from the history Sonny
//! replays: its `ask` calls not yet answered nor closed.
//!
//! Silence counts as a yes only where the agent says a step can be undone,
//! as an Apache project's lazy consensus passes what is easy to revert once
//! its stated time is up. The agent marks each ask `go_now`, `go_later` or
//! `wait`, the card says which, and for the last two the tool host that
//! answers the call ([`served`]) sets a one-off trigger at the deadline.
//! Its `--if` is `sonny ask open` ([`still_open`]): once the person
//! answered, or wrote words of their own to that agent, the conversation
//! moved on and nothing fires. A fire is Sonny's words in a trigger's turn,
//! never the person's, so it allows nothing that needs their own; a pick
//! of another option later still answers the card, and the agent undoes
//! what it did.
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;

/// The name of the tool the lead asks with.
pub const TOOL: &str = "ask";
/// The name of the tool an agent closes a question of its own with.
pub const CLOSE: &str = "close_ask";
/// Options are picked by one digit.
const OPTIONS: usize = 9;
/// The mark a decision starts with, and the bar down its left side.
const GLYPH: &str = "? ";
const BAR: &str = "┃ ";
/// The marks an answered question and a withdrawn one start with.
const TICK: &str = "✓ ";
const DASH: &str = "– ";
/// The longest wait an agent may set: a week.
const MAX_MINUTES: u64 = 7 * 24 * 60;
/// How much of the question a fire's message quotes, in characters.
const QUOTED: usize = 300;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Ask {
    /// What it is, as [`id`] names it: the agent, the turn and the call
    /// that asked it.
    pub id: String,
    pub question: String,
    /// Why it is asked: what the answer decides.
    pub why: String,
    pub options: Vec<Choice>,
    /// For a step only the person can take, in place of options: the
    /// exact line for them to run in their own terminal.
    pub command: Option<String>,
    /// The option recommended, from 0, and why.
    pub recommended: Option<usize>,
    pub reason: String,
    /// What the agent does while nobody answers, as Sonny took it: none
    /// for an ask from before agents said.
    pub unanswered: Option<Unanswered>,
    /// When it goes ahead or is raised again, in seconds since the epoch,
    /// as the call's result says.
    pub deadline: Option<i64>,
    /// For an ask that approves an action, what a yes covers.
    pub covers: Option<Covers>,
    /// The task that asked, when not the window's own agent: a lead's
    /// window shows its tasks' open questions too, and an answer to one
    /// goes to the task, the same answer to the same ask its own window
    /// sends.
    pub task: Option<Asker>,
}

impl Ask {
    /// Which open question this is in a window: its id, and for a task's
    /// the task too, as a task and its fork ask under the same id (a
    /// fork keeps its source's turns) and each takes its own answer. Bot
    /// names hold no `@` or `/`.
    pub fn key(&self) -> String {
        match &self.task {
            Some(task) => format!("@{}/{}", task.bot, self.id),
            None => self.id.clone(),
        }
    }
}

/// Where a question stands, as its card says it: one of open, set aside,
/// answered or withdrawn, answered either by the person or, from their
/// words, by the agent's own record.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// It waits on the person.
    Open,
    /// The person wrote words of their own past it: out of the composer
    /// and the count, a pick still answering it.
    Aside,
    /// The person answered it: the option they picked, or their words.
    Answered(String),
    /// The agent recorded that the person's words answered it, going with
    /// an option (from 0) or none, and its note of what they said. Its
    /// record, never their answer: a pick still answers it.
    Recorded { option: Option<usize>, note: String },
    /// The agent took it back as no longer needed, and why.
    Withdrawn(String),
}

impl State {
    /// Whether it is answered or closed: its card folds, and it waits on
    /// nobody.
    pub fn settled(&self) -> bool {
        !matches!(self, Self::Open | Self::Aside)
    }

    /// Whether a pick on its card still answers it: anything but the
    /// person's own answer, which the daemon takes once.
    pub fn takes_a_pick(&self) -> bool {
        !matches!(self, Self::Answered(_))
    }

    /// The same state with its words cut to a line, as kept for a card
    /// no longer drawn: what it shows once drawn again, at a size that
    /// does not grow with what was said.
    pub fn kept(self) -> Self {
        let cut = |text: String| crate::transcript::cut(text.lines().next().unwrap_or(""), QUOTED);
        match self {
            Self::Answered(text) => Self::Answered(cut(text)),
            Self::Recorded { option, note } => Self::Recorded {
                option,
                note: cut(note),
            },
            Self::Withdrawn(text) => Self::Withdrawn(cut(text)),
            other => other,
        }
    }
}

/// What a `close_ask` call with `arguments` closes: the ask it names, as
/// [`id`] names it, and the state it leaves it in.
pub fn closing(arguments: &Value) -> Option<(String, State)> {
    let asked = arguments["id"].as_str()?.trim();
    if !well_formed(asked) {
        return None;
    }
    let note = tidy(arguments["note"].as_str()).replace('\n', " ");
    let state = match arguments["outcome"].as_str()? {
        "answered" => State::Recorded {
            option: (arguments["option"].as_u64()).and_then(|n| (n as usize).checked_sub(1)),
            note,
        },
        "withdrawn" => State::Withdrawn(note),
        _ => return None,
    };
    Some((asked.to_owned(), state))
}

/// The ask a history `item` closes, when it is a `close_ask` call.
pub fn closed_by(item: &Value) -> Option<(String, State)> {
    if item["type"] != "function_call" || item["name"] != CLOSE {
        return None;
    }
    let arguments = serde_json::from_str::<Value>(item["arguments"].as_str()?).ok()?;
    closing(&arguments)
}

/// A task whose question a lead's window shows: the agent its answer goes
/// to, and its name as the window says it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Asker {
    pub bot: String,
    pub name: String,
}

/// What a yes to an ask covers: one action, on one target, at one
/// revision where the target has one. A yes covers that and nothing else,
/// as a review's approval covers the commit it saw, so another target or
/// revision needs another ask.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Covers {
    pub action: String,
    pub target: String,
    pub revision: Option<String>,
}

impl Covers {
    /// What an ask's `covers` names, when it names an action and a target.
    fn of(covers: &Value) -> Option<Self> {
        let word = |key: &str| {
            let word = tidy(covers[key].as_str()).replace('\n', " ");
            (!word.is_empty()).then_some(word)
        };
        Some(Self {
            action: word("action")?,
            target: word("target")?,
            revision: word("revision"),
        })
    }

    /// It in one line, as the card shows it and the answer repeats it:
    /// `publish · lydakis/agent#12 · at 3f2a9c1`.
    pub fn line(&self) -> String {
        match &self.revision {
            Some(revision) => format!("{} · {} · at {revision}", self.action, self.target),
            None => format!("{} · {}", self.action, self.target),
        }
    }
}

/// What an agent does while the person has not answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Unanswered {
    /// Goes on with what it recommends at once: a step it can undo.
    Now,
    /// Goes on with it once the deadline passes with no word from them.
    Later,
    /// Does nothing they have not picked, and is reminded at the deadline.
    Wait,
}

impl Unanswered {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "go_now" => Some(Self::Now),
            "go_later" => Some(Self::Later),
            "wait" => Some(Self::Wait),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Now => "go_now",
            Self::Later => "go_later",
            Self::Wait => "wait",
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Choice {
    pub label: String,
    /// What picking it means.
    pub explanation: String,
}

/// An ask's identity, `ask:TURN/CALL`: the turn it was asked in and the
/// call. A provider's call IDs are unique only within a turn, so the call
/// alone may name two asks; the daemon numbers turns across every agent,
/// and a fork keeps its source's turns, so the turn names the agent that
/// asked too, read from the source or any fork of it alike.
pub fn id(turn: i64, call: &str) -> String {
    format!("ask:{turn}/{call}")
}

/// The turn that asked ask `id`, as [`id`] names it: turns count up
/// across every agent, so the older ask has the smaller.
pub fn turn_of(id: &str) -> Option<i64> {
    id.strip_prefix("ask:")?.split_once('/')?.0.parse().ok()
}

/// The `request_id` the person's answer to ask `id` goes with on its
/// `attempt`th sending, from 0: the same from every window and on every
/// resend, so the daemon takes one answer to an ask and refuses another
/// (`idempotency_conflict`), and two windows showing it can't both answer.
/// An answer taken back from the queue before the agent read it answered
/// nothing ([`undelivered`]): the next one goes as the next attempt.
pub fn request_id(id: &str, attempt: u32) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    match attempt {
        0 => format!("answer-{hex}"),
        attempt => format!("answer-{hex}.{attempt}"),
    }
}

/// The most attempts one ask's answer is sent under, each after the one
/// before was taken back unread.
pub const ATTEMPTS: u32 = 1000;

/// Whether a message, as `messages` lists it, ended before it reached the
/// agent: withdrawn or bounced while it waited in line. An answer so ended
/// answered nothing.
pub fn undelivered(message: &Value) -> bool {
    matches!(message["status"].as_str(), Some("withdrawn" | "failed")) && message["turn"].is_null()
}

/// The `ask` tool as Sonny declares it on a bot, answered by its tool
/// host, which sets the deadline.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Put a decision in front of the person: a fork in the road, an architectural \
            choice, or a step that cannot be undone, where their pick changes what you build. Sonny \
            shows it where you ask it; say what each option does to their goal. Check `sonny ask \
            list` first: an answer there may settle yours. Ask once, one decision per call, and do \
            not repeat it in your reply. When their pick approves an \
            action, say what in covers: their yes covers that action, target and revision alone, so \
            a new target or revision needs a new ask. \
            Their answer, a pick or words typed for it, comes as their next message, starting \
            \"You asked\", the ask's ID, your question and what it covers; other words come as \
            written: once they answer it, or it no longer matters, close it with close_ask. Say in \
            if_unanswered what you do until then. go_now goes on with the option you recommend at \
            once, for a step you can undo once they have seen it. go_later goes on with it once \
            the wait passes with no word from them, for a step you can undo but would rather they \
            saw first. wait does nothing they have not picked: anything that goes out in their \
            name or to other people, approves, merges, publishes, deletes, spends or cannot be \
            undone waits, and so does an ask you are unsure of or one they said to wait on. Sonny \
            wakes you at the deadline unless they answered or wrote to you since; that wake is \
            Sonny's, never their answer, so it allows nothing that needs their own words. If they \
            pick another option after you went ahead, undo what you did and follow their pick. A step only \
            the person can take, such as a sign-in or a command needing their own terminal, is an \
            ask too: no options, put the exact line to run in command, what it unblocks in why, and \
            wait; their next message says how it went. Any other question with nothing to choose \
            between goes in your reply.",
        "parameters": {
            "type": "object",
            "properties": {
                "question": {"type": "string",
                    "description": "The question, verb first, in the person's own words, in about twelve words."},
                "why": {"type": "string",
                    "description": "What the answer decides and what the person needs to know to choose, in a sentence or three."},
                "options": {"type": "array",
                    "description": "2 to 4 options, each a different way forward; none with command.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {"type": "string", "description": "The option, in a few words."},
                            "explanation": {"type": "string",
                                "description": "What happens if the person picks it, in one sentence."}
                        },
                        "required": ["label", "explanation"],
                        "additionalProperties": false
                    }},
                "command": {"type": "string",
                    "description": "In place of options: the exact line to paste into their \
                        terminal, whole, nothing to fill in."},
                "recommended": {"type": "integer",
                    "description": "The number of the option you recommend, from 1; with options."},
                "reason": {"type": "string",
                    "description": "Why you recommend it, in one sentence; with options."},
                "if_unanswered": {"type": "string", "enum": ["go_now", "go_later", "wait"],
                    "description": "What you do until they answer, as above."},
                "covers": {"type": "object",
                    "description": "What a yes approves.",
                    "properties": {
                        "action": {"type": "string",
                            "description": "A word or two: publish, merge, send, delete."},
                        "target": {"type": "string",
                            "description": "What it acts on: a pull request, a path."},
                        "revision": {"type": "string",
                            "description": "The exact version, where one exists: a commit, a draft, \
                                a hash."}
                    },
                    "required": ["action", "target"],
                    "additionalProperties": false},
                "wait_minutes": {"type": "integer", "minimum": 1, "maximum": MAX_MINUTES,
                    "description": "How long go_later waits, or wait before a reminder, for work \
                        on a clock such as a release; left out, the person's setting."}
            },
            "required": ["question", "why", "if_unanswered"],
            "additionalProperties": false
        },
        "answered_by": crate::served::TAG,
        "expire_ms": 60 * 1000
    })
}

/// The `close_ask` tool as Sonny declares it on a bot, answered by its
/// tool host, which checks the ask is the agent's and still waits.
pub fn close_tool() -> Value {
    json!({
        "name": CLOSE,
        "description": "Close a question you asked that no longer waits on the person; its card \
            shows the outcome and your note. answered: their words answered it, with the option \
            you go with. withdrawn: it no longer matters. A later pick of theirs still answers it.",
        "parameters": {
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "The ID ask gave."},
                "outcome": {"type": "string", "enum": ["answered", "withdrawn"]},
                "option": {"type": "integer", "minimum": 1},
                "note": {"type": "string", "description": "One line: what they said, or why."}
            },
            "required": ["id", "outcome", "note"],
            "additionalProperties": false
        },
        "answered_by": crate::served::TAG,
        "expire_ms": 60 * 1000
    })
}

/// What the agent wrote to be read, trimmed, its paragraphs kept apart
/// by one blank line.
fn prose(text: Option<&str>) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.unwrap_or("").lines().map(str::trim_end) {
        if !(line.is_empty() && out.last().is_none_or(|last| last.is_empty())) {
            out.push(line);
        }
    }
    out.join("\n").trim().to_owned()
}

/// `text` trimmed, without blank lines, so an answer that quotes it
/// still reads as answering it.
fn tidy(text: Option<&str>) -> String {
    text.unwrap_or("")
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// The question an `ask` call with `arguments` put, as ask `id` ([`id`]).
pub fn call(id: &str, arguments: &Value) -> Option<Ask> {
    let question = tidy(arguments["question"].as_str());
    let options: Vec<Choice> = arguments["options"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|option| Choice {
            label: tidy(option["label"].as_str()),
            explanation: prose(option["explanation"].as_str()),
        })
        .filter(|choice| !choice.label.is_empty())
        .take(OPTIONS)
        .collect();
    // The line to run as the agent wrote it, only its ends trimmed: a
    // blank line or a space inside a quoted value or a here-document is
    // part of what it does.
    let command = (arguments["command"].as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(str::to_owned);
    let recommended = arguments["recommended"]
        .as_u64()
        .and_then(|n| (n as usize).checked_sub(1))
        .filter(|at| *at < options.len());
    // Going ahead goes ahead with a recommendation; with none it waits.
    let unanswered = (arguments["if_unanswered"].as_str())
        .and_then(Unanswered::parse)
        .map(|plan| {
            if recommended.is_some() {
                plan
            } else {
                Unanswered::Wait
            }
        });
    (!question.is_empty()).then(|| Ask {
        id: id.to_owned(),
        question,
        why: prose(arguments["why"].as_str()),
        options,
        command,
        recommended,
        reason: prose(arguments["reason"].as_str()),
        unanswered,
        deadline: None,
        covers: Covers::of(&arguments["covers"]),
        task: None,
    })
}

/// `ask` as its call's result `output` says Sonny took it: what the agent
/// does meanwhile, and the deadline Sonny set.
pub fn settled(mut ask: Ask, output: &str) -> Ask {
    let Ok(said) = serde_json::from_str::<Value>(output) else {
        return ask;
    };
    if let Some(plan) = said["if_unanswered"].as_str().and_then(Unanswered::parse) {
        ask.unanswered = Some(plan);
    }
    ask.deadline = said["deadline_s"].as_i64();
    ask
}

/// The oldest question still open among `bot`'s `items`, newest first,
/// whole, as its card shows it, under the ID [`id`] gives it from the
/// `turn` each call is marked with: an `ask` call that was
/// taken, with no answer to it after, as the pinned questions are closed:
/// by the person's `You asked` message naming that ask, one answer to one
/// ask, or by words of their own after it, the conversation going on past
/// it. Asks are told apart by turn and call, never by wording: two
/// asking the same question are two decisions.
pub fn open(items: &[Value]) -> Option<Ask> {
    let (mut answers, mut refused, mut open) = (Vec::new(), Vec::new(), None);
    // Each call's result, by turn and call, as provider IDs repeat across
    // turns: what Sonny took an ask as, its plan and deadline, is there.
    let mut results: Vec<((Value, Value), String)> = Vec::new();
    let mut moved_on = false;
    for item in items {
        let at = (item["turn"].clone(), item["call_id"].clone());
        if item["role"] == "user" {
            let text = crate::text(item);
            match answer_of(&text, item["from"].as_str()) {
                Some(call) => answers.push(call.to_owned()),
                None => moved_on |= moves_on(&text, item["from"].as_str()),
            }
        }
        if item["type"] == "function_call_output" {
            match item["failed"] == true || crate::reactions::refused(item) {
                true => refused.push(at.clone()),
                false => results.push((at.clone(), crate::output(item).0)),
            }
        }
        // The agent closed it, as an answer would, once its close was
        // taken: one still running may yet fail.
        if let Some((closed, _)) = closed_by(item)
            && results.iter().any(|(of, _)| *of == at)
        {
            answers.push(closed);
        }
        if item["type"] == "function_call"
            && item["name"] == TOOL
            && !refused.contains(&at)
            && let Some(mut ask) = item["arguments"]
                .as_str()
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .and_then(|arguments| call("", &arguments))
        {
            let asked = (item["turn"].as_i64())
                .zip(item["call_id"].as_str())
                .map(|(turn, call)| id(turn, call));
            match (answers.iter()).position(|answered| Some(answered) == asked.as_ref()) {
                Some(at) => {
                    answers.remove(at);
                }
                None if !moved_on => {
                    // One whose turn is not known no answer names.
                    ask.id = (asked.or_else(|| item["call_id"].as_str().map(str::to_owned)))
                        .unwrap_or_default();
                    if let Some((_, said)) = results.iter().find(|(of, _)| *of == at) {
                        ask = settled(ask, said);
                    }
                    open = Some(ask);
                }
                None => {}
            }
        }
    }
    open
}

/// Whether a history item bears on the asks [`decisions`] reads: an ask
/// call, a call's failed or refused result, or the person's answer.
pub fn bears_on_asks(item: &Value) -> bool {
    match item["type"].as_str() {
        Some("function_call") => item["name"] == TOOL,
        Some("function_call_output") => item["failed"] == true || crate::reactions::refused(item),
        _ => {
            item["role"] == "user" && answer_of(&crate::text(item), item["from"].as_str()).is_some()
        }
    }
}

/// The asks among a bot's history `items`, newest first, each with the
/// `turn` it holds, as the approver's judge is shown them: each ask's ID,
/// question and what a yes covers, and the person's pick with the turn it
/// came in, or none while it is open. An answer names its ask whole
/// ([`answer_of`]): another's words, or an old-form answer, answer none. A
/// refused or failed ask was never put to the person.
pub fn decisions(items: &[(i64, Value)]) -> Vec<agent_client::approver::Decision> {
    let mut answers: Vec<(String, i64, String)> = Vec::new();
    // By turn and call: a provider's call IDs repeat across turns.
    let mut refused: Vec<(i64, Value)> = Vec::new();
    let mut decisions = Vec::new();
    for (turn, item) in items {
        if item["role"] == "user" {
            let text = crate::text(item);
            if let Some(ask) = answer_of(&text, item["from"].as_str())
                && let Some((_, pick)) = split(&text)
            {
                answers.push((ask.to_owned(), *turn, pick.trim().to_owned()));
            }
        }
        if item["type"] == "function_call_output"
            && (item["failed"] == true || crate::reactions::refused(item))
        {
            refused.push((*turn, item["call_id"].clone()));
        }
        if item["type"] != "function_call"
            || item["name"] != TOOL
            || refused.contains(&(*turn, item["call_id"].clone()))
        {
            continue;
        }
        let Some(call) = item["call_id"].as_str() else {
            continue;
        };
        let asked = id(*turn, call);
        let Some(ask) = (item["arguments"].as_str())
            .and_then(|a| serde_json::from_str::<Value>(a).ok())
            .and_then(|arguments| self::call(&asked, &arguments))
        else {
            continue;
        };
        // The first answer counts, as the daemon takes only the first.
        let answered = (answers.iter().rev())
            .find(|(answering, ..)| *answering == asked)
            .map(|(_, turn, pick)| (*turn, pick.clone()));
        decisions.push(agent_client::approver::Decision {
            id: asked,
            question: ask.question,
            covers: ask.covers.map(|covers| agent_client::approver::Covers {
                action: covers.action,
                target: covers.target,
                revision: covers.revision,
            }),
            answered,
        });
    }
    decisions
}

/// Whether a message is the person's own words, answering no question:
/// not another agent's, nor a reaction, a note taken away or what they
/// ran. After it the questions asked before are set aside, the
/// conversation having gone on past them, as a chat's thread does.
pub fn moves_on(text: &str, from: Option<&str>) -> bool {
    answered(text).is_none()
        && !crate::transcript::relays(text, from)
        && !crate::reactions::is_reaction(text)
        && crate::margin::closed(text).is_none()
        && crate::user_shell::read(text).is_none()
}

/// The ask a person's message answers, as [`id`] names it, when
/// [`answer`] wrote it.
pub fn answered(text: &str) -> Option<&str> {
    parts(text).map(|(id, ..)| id).filter(|id| well_formed(id))
}

/// The ask a message answers, sent `from` another agent, a client or
/// another person, or by the person when none: only the person answers
/// an ask, so the same words from anyone else answer none. Every reader
/// of asks (the window, history, an ask's deadline) goes by this.
pub fn answer_of<'a>(text: &'a str, from: Option<&str>) -> Option<&'a str> {
    from.is_none().then(|| answered(text)).flatten()
}

/// Whether `id` names an ask whole: turn and call. An answer sent before
/// asks named their turn names the call alone, which may be another
/// turn's: it answers none, and reads as the person's own words.
fn well_formed(id: &str) -> bool {
    id.strip_prefix("ask:")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(turn, call)| turn.parse::<i64>().is_ok() && !call.is_empty())
}

/// A person's message [`answer`] wrote, as the question and its answer,
/// for the transcript to draw it so, whichever form named its ask.
pub fn split(text: &str) -> Option<(&str, &str)> {
    parts(text).map(|(_, question, answer)| (question, answer))
}

/// A person's message [`answer`] wrote, as the ask it names, the question
/// and the answer, past the line saying what it covers.
fn parts(text: &str) -> Option<(&str, &str, &str)> {
    let (id, rest) = text.strip_prefix("You asked (")?.split_once("): ")?;
    let (question, answer) = rest.split_once("\n\n")?;
    let question = (question.rsplit_once(COVERS)).map_or(question, |(question, _)| question);
    (!id.is_empty() && !id.contains(char::is_whitespace)).then_some((id, question, answer))
}

/// The line an answer says what its yes covers on, under the question.
const COVERS: &str = "\nCovers: ";

/// The row that says which question a message answers, above it.
pub fn answering(question: &str, width: usize) -> Line<'static> {
    answering_row(question, "", width)
}

/// The row that names the question answered, as its card reads it,
/// then `place`.
fn answering_row(question: &str, place: &str, width: usize) -> Line<'static> {
    let room = width.saturating_sub(14 + place.chars().count()).max(8);
    let cut = crate::transcript::cut(&plain(question), room);
    Line::from(format!("  answering “{cut}”{place}")).dim()
}

/// The option `typed` picks by its number, if it is one.
fn option<'a>(ask: &'a Ask, typed: &str) -> Option<&'a Choice> {
    typed
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|at| ask.options.get(at))
}

/// Whether `typed` is an option's number, a pick, rather than words.
pub fn picks(ask: &Ask, typed: &str) -> bool {
    option(ask, typed).is_some()
}

/// The person's message answering `ask`, by its call, so it answers that
/// ask and no other asking the same: an option's number stands for the
/// option.
pub fn answer(ask: &Ask, typed: &str) -> String {
    let picked = option(ask, typed).map_or(typed, |choice| choice.label.as_str());
    let covers =
        (ask.covers.as_ref()).map_or_else(String::new, |c| format!("{COVERS}{}", c.line()));
    format!(
        "You asked ({}): {}{covers}\n\n{picked}",
        ask.id, ask.question
    )
}

/// The decision as it is drawn where it was asked, as [`card`] lays it.
pub struct Card {
    pub rows: Vec<Line<'static>>,
    /// The row each option starts on, which a click picks it on.
    pub options: Vec<usize>,
    /// The rows the command to run is on, which a click copies it from.
    pub command: std::ops::Range<usize>,
    /// What each row's rows past its first start with, should it be
    /// drawn narrower than it was laid: the bar, then room to the text,
    /// so the bar goes on down however the rows wrap.
    pub hangs: Vec<Line<'static>>,
    /// The rows the question is on, a click on which brings it to the
    /// composer, or opens or folds a settled card.
    pub question: std::ops::Range<usize>,
}

/// The decision as it is drawn where it was asked: the question, why,
/// each option by number with what it means, and the recommendation.
/// A `?` in yellow, what needs the person, starts it, and a muted bar
/// down its left side sets it apart from the replies around it, as a
/// callout's does: a shape of its own without color too. Where it
/// stands, when not open, is its last row; set aside, the `?` is muted;
/// answered it starts with a `✓`, withdrawn with a `–`, and `folded`
/// it is the question and that row alone.
pub fn card(ask: &Ask, state: &State, folded: bool, width: usize) -> Card {
    // Prose reads at most so wide, as a reply does, labels too.
    let width = width.min(crate::measure());
    let mut options = Vec::new();
    let indent = || Span::raw(BAR).dim();
    // What the agent wrote reads as its Markdown, as a reply does; the
    // bar keeps its own look. Each row hangs under its text, past the
    // `lead` columns every row of the part starts with.
    let markdown = |text: &str, first: Line<'static>, rest: Line<'static>, style: Style| {
        let lead = rest.width();
        let mut markdown = crate::transcript::Markdown::default();
        let rows = crate::transcript::markdown(&mut markdown, text, width, (first, rest));
        rows.into_iter().map(move |mut row| {
            for span in (row.spans.iter_mut())
                .filter(|span| ![BAR, GLYPH, TICK, DASH].contains(&&*span.content))
            {
                span.style = style.patch(span.style);
            }
            let hang = hang(&row, lead);
            (row, hang)
        })
    };
    let mut rows: Vec<(Line<'static>, Line<'static>)> = Vec::new();
    let spacer = || (Line::from(indent()), Line::from(indent()));
    // What needs the person in yellow and bold; what no longer does
    // muted, a settled question at the text's own weight.
    let (glyph, weight) = match state {
        State::Open => (
            Span::raw(GLYPH).fg(Color::Yellow).bold(),
            Style::new().bold(),
        ),
        State::Aside => (Span::raw(GLYPH).dim(), Style::new().bold()),
        State::Answered(_) | State::Recorded { .. } => {
            (Span::raw(TICK).fg(Color::Green), Style::new())
        }
        State::Withdrawn(_) => (Span::raw(DASH).dim(), Style::new()),
    };
    rows.extend(markdown(
        &ask.question,
        Line::from(glyph),
        Line::from(indent()),
        weight,
    ));
    let question = 0..rows.len();
    // Where it stands, under the rest, muted, what was answered at the
    // text's own weight.
    let stands = |rows: &mut Vec<(Line<'static>, Line<'static>)>| {
        if let Some(spans) = stands(ask, state) {
            let wrapped = crate::transcript::wrap_lines(
                Line::from(spans),
                width,
                Line::from(indent()),
                Line::from(indent()),
            );
            rows.extend(wrapped.into_iter().map(|row| (row, Line::from(indent()))));
        }
    };
    if folded && state.settled() {
        stands(&mut rows);
        let (rows, hangs) = rows.into_iter().unzip();
        return Card {
            rows,
            options,
            command: 0..0,
            hangs,
            question,
        };
    }
    rows.extend(markdown(
        &ask.why,
        Line::from(indent()),
        Line::from(indent()),
        Style::new().dim(),
    ));
    // What a yes covers, muted under the why, as the answer repeats it.
    if let Some(covers) = &ask.covers {
        let row = Line::from(format!("Covers: {}", covers.line())).dim();
        let wrapped =
            crate::transcript::wrap_lines(row, width, Line::from(indent()), Line::from(indent()));
        rows.extend(wrapped.into_iter().map(|row| (row, Line::from(indent()))));
    }
    if !ask.options.is_empty() || ask.command.is_some() {
        rows.push(spacer());
    }
    // A step for the person: the line to run at the text's own weight,
    // under a muted row saying where, as a code block reads in a reply.
    let mut command = 0..0;
    if let Some(line) = &ask.command {
        let said = Line::from("Run this in your own terminal; a click copies it.").dim();
        let said =
            crate::transcript::wrap_lines(said, width, Line::from(indent()), Line::from(indent()));
        rows.extend(said.into_iter().map(|row| (row, Line::from(indent()))));
        let under = || Line::from(vec![indent(), Span::raw("   ")]);
        let from = rows.len();
        for part in line.lines() {
            let wrapped =
                crate::transcript::wrap_lines(Line::from(part.to_owned()), width, under(), under());
            rows.extend(wrapped.into_iter().map(|row| (row, under())));
        }
        command = from..rows.len();
    }
    // The explanation sits under its label, past the number.
    let under = || vec![indent(), Span::raw("   ")];
    for (at, choice) in ask.options.iter().enumerate() {
        options.push(rows.len());
        let mut label = vec![Span::raw(choice.label.clone()).bold()];
        if ask.recommended == Some(at) {
            label.push(Span::raw(" · recommended").dim());
        }
        let label = crate::transcript::wrap_lines(
            Line::from(label),
            width,
            Line::from(vec![indent(), Span::raw(format!("{:<3}", at + 1))]),
            Line::from(under()),
        );
        rows.extend(label.into_iter().map(|row| (row, Line::from(under()))));
        rows.extend(markdown(
            &choice.explanation,
            Line::from(under()),
            Line::from(under()),
            Style::new(),
        ));
    }
    if let Some(at) = ask.recommended
        && !ask.reason.is_empty()
    {
        rows.push(spacer());
        rows.extend(markdown(
            &ask.reason,
            Line::from(vec![indent(), Span::raw(format!("Why {}: ", at + 1))]),
            Line::from(indent()),
            Style::new().dim(),
        ));
    }
    // What happens while nobody answers, under the recommendation it
    // follows, muted as the reason is, the time at the text's own weight:
    // only while it is open, past which nothing fires.
    if *state != State::Open {
        if ask.reason.is_empty() || ask.recommended.is_none() {
            rows.push(spacer());
        }
        stands(&mut rows);
    } else if let Some(meanwhile) = meanwhile(ask, now()) {
        if ask.reason.is_empty() {
            rows.push(spacer());
        }
        let spans = meanwhile.into_iter().map(|(text, plain)| match plain {
            true => Span::raw(text),
            false => Span::raw(text).dim(),
        });
        let row = Line::from(spans.collect::<Vec<_>>());
        let wrapped =
            crate::transcript::wrap_lines(row, width, Line::from(indent()), Line::from(indent()));
        rows.extend(wrapped.into_iter().map(|row| (row, Line::from(indent()))));
    }
    let (rows, hangs) = rows.into_iter().unzip();
    Card {
        rows,
        options,
        command,
        hangs,
        question,
    }
}

/// Where a question stands, as the last row of its card says it: muted
/// words, what was answered or noted at the text's own weight; none while
/// it is open.
fn stands(ask: &Ask, state: &State) -> Option<Vec<Span<'static>>> {
    let muted = |text: &str| Span::raw(text.to_owned()).dim();
    let plain = |text: &str| Span::raw(first_line(text));
    let noted = |label: &str, note: &str| match note.is_empty() {
        true => vec![muted(label)],
        false => vec![muted(&format!("{label} · ")), plain(note)],
    };
    Some(match state {
        State::Open => return None,
        State::Aside => vec![muted(
            "Set aside: you wrote past it. A pick still answers it.",
        )],
        State::Answered(answer) => vec![muted("Answered · "), plain(answer)],
        State::Recorded { option, note } => {
            let label = "Answered in your words";
            match option.and_then(|at| ask.options.get(at)) {
                Some(choice) => {
                    let mut spans = vec![
                        muted(&format!("{label} · going with ")),
                        plain(&choice.label),
                    ];
                    if !note.is_empty() {
                        spans.extend([muted(" · "), plain(note)]);
                    }
                    spans
                }
                None => noted(label, note),
            }
        }
        State::Withdrawn(note) => noted("Withdrawn", note),
    })
}

/// The first line of `text` with words, trimmed, and `…` after it when
/// more follow.
fn first_line(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default().to_owned();
    match lines.next() {
        Some(_) => format!("{first} …"),
        None => first,
    }
}

/// The lead a card's `row` goes on under when wrapped again: its bar,
/// then room past the `lead` columns to its text, a list's marker or a
/// quote's bar being lead too, the quote's bar kept.
fn hang(row: &Line<'static>, lead: usize) -> Line<'static> {
    // The row's characters past its lead, each with whether it is a
    // quote's bar, a tinted blank.
    let after: Vec<(char, bool)> = (row.spans.iter())
        .flat_map(|span| {
            let bar = crate::transcript::is_bar(span);
            span.content.chars().map(move |c| (c, bar))
        })
        .skip(lead)
        .collect();
    let spaces = (after.iter())
        .take_while(|(c, bar)| *c == ' ' && !bar)
        .count();
    let rest: String = after[spaces..].iter().map(|(c, _)| c).collect();
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let marker = match rest.as_str() {
        _ if ["- ", "* ", "+ ", "• "].iter().any(|m| rest.starts_with(m)) => 2,
        _ if digits > 0 && [". ", ") "].iter().any(|m| rest[digits..].starts_with(m)) => digits + 2,
        _ => 0,
    };
    let mut spans = vec![
        Span::raw(BAR).dim(),
        Span::raw(" ".repeat(lead - BAR.width())),
    ];
    // Under a quote its bars go on too, as deep as it is.
    let depth = (after[spaces..].chunks(2))
        .take_while(|pair| matches!(pair, [(_, true), (' ', false)]))
        .count();
    match depth {
        0 => spans.push(Span::raw(" ".repeat(spaces + marker))),
        _ => {
            spans.push(Span::raw(" ".repeat(spaces)));
            spans.extend(crate::transcript::bars(depth));
        }
    }
    Line::from(spans)
}

/// What the card says the agent does until the person answers, `now`
/// in seconds since the epoch: words, each muted or at the text's weight.
fn meanwhile(ask: &Ask, now: i64) -> Option<Vec<(String, bool)>> {
    let pick = ask.recommended.map(|at| (at + 1).to_string());
    let muted = |text: &str| (text.to_owned(), false);
    let at = |deadline: i64| (clock(deadline, now), true);
    Some(match (ask.unanswered?, pick, ask.deadline) {
        (Unanswered::Now, Some(pick), _) => vec![muted(&format!(
            "Going with {pick} meanwhile; another pick undoes it."
        ))],
        (Unanswered::Later, Some(pick), Some(deadline)) => vec![
            muted(&format!("Goes with {pick} at ")),
            at(deadline),
            muted(" unless you pick."),
        ],
        (Unanswered::Wait, _, Some(deadline)) if ask.command.is_some() => vec![
            muted("Waits for you to run it; a reminder at "),
            at(deadline),
            muted("."),
        ],
        (Unanswered::Wait, _, Some(deadline)) => vec![
            muted("Waits for your pick; a reminder at "),
            at(deadline),
            muted("."),
        ],
        // No deadline: the person's setting turned them off, or Sonny
        // could not say when.
        _ if ask.command.is_some() => vec![muted("Waits for you to run it.")],
        _ => vec![muted("Waits for your pick.")],
    })
}

/// `epoch` as a clock shows it: the time today, else tomorrow's or the
/// date's, as `11-03 09:30`.
pub fn clock(epoch: i64, now: i64) -> String {
    let (at, today) = (crate::trigger::local(epoch), crate::trigger::local(now));
    let time = format!("{:02}:{:02}", at.hour, at.minute);
    let tomorrow = crate::trigger::local(now + 24 * 3600);
    let day = |l: &crate::trigger::Local| (l.year, l.month, l.day);
    if day(&at) == day(&today) {
        time
    } else if day(&at) == day(&tomorrow) {
        format!("tomorrow {time}")
    } else {
        format!("{:02}-{:02} {time}", at.month, at.day)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Sonny's answer to an `ask` call by `bot` in its turn `turn` as call
/// `call`, run in that agent's shell: what it does until the person
/// answers, and for `go_later` and `wait` the one-off trigger that wakes
/// it at the deadline, `wait_minutes` or the person's setting, which at 0
/// lets every ask wait with no deadline. The result says when, for the
/// card and the agent.
pub fn served(arguments: &Value, bot: &str, turn: i64, call: &str) -> Result<String, String> {
    let ask = self::call(&id(turn, call), arguments)
        .ok_or("invalid_arguments: a question to ask, in question")?;
    let asked = arguments["if_unanswered"].as_str().unwrap_or_default();
    let plan = Unanswered::parse(asked)
        .ok_or("invalid_arguments: if_unanswered is go_now, go_later or wait")?;
    if ask.command.is_some() && !ask.options.is_empty() {
        return Err(
            "invalid_arguments: a command is a step for the person to take, with no options; ask \
             for one or the other"
                .into(),
        );
    }
    if ask.command.is_none() && ask.options.is_empty() {
        return Err(
            "invalid_arguments: options to pick from, or a command for a step only the person can \
             take"
                .into(),
        );
    }
    // A step only the person can take waits for them, whatever was asked.
    let plan = if ask.command.is_some() {
        Unanswered::Wait
    } else {
        plan
    };
    if plan != Unanswered::Wait && ask.recommended.is_none() {
        return Err(
            "invalid_arguments: going ahead needs the option you recommend, in recommended".into(),
        );
    }
    let label = ask
        .recommended
        .map(|at| ask.options[at].label.clone())
        .unwrap_or_default();
    let setting = agent_client::config::load()
        .map(|config| config.sonny.ask_wait_minutes)
        .unwrap_or(60);
    let minutes = match arguments["wait_minutes"].as_u64() {
        _ if setting == 0 => 0,
        Some(minutes) => minutes.clamp(1, MAX_MINUTES),
        None => setting.min(MAX_MINUTES),
    };
    let plan = allowed(plan, minutes);
    let deadline = (plan != Unanswered::Now && minutes > 0).then(|| deadline(minutes, now()));
    // A deadline Sonny could not set leaves the ask waiting, and says why:
    // the card still shows, and nothing goes ahead unseen.
    let (plan, deadline, unset) =
        match deadline.map(|at| set_deadline(&ask, plan, &label, at, bot, turn, call)) {
            Some(Err(error)) => (Unanswered::Wait, None, Some(error)),
            _ => (plan, deadline, None),
        };
    let when = deadline.map(|d| clock(d, now()));
    let next = match (plan, &when, &unset) {
        _ if ask.command.is_some() => format!(
            "Shown to the person with the command to run. Do nothing that needs it until they say \
             how it went, in their next message.{} Meanwhile do work that does not depend on it, \
             or end your turn.",
            match &when {
                Some(when) =>
                    format!(" Unless they write to you first, Sonny reminds you at {when}."),
                None => String::new(),
            }
        ),
        (Unanswered::Now, ..) => format!(
            "Shown to the person. Go on now with \"{label}\", keeping it undoable; if they pick \
             another, undo it and follow their pick."
        ),
        (Unanswered::Later, Some(when), _) => format!(
            "Shown to the person. Unless they answer or write to you first, Sonny wakes you at \
             {when} to go on with \"{label}\". Until then do work that does not depend on the \
             answer, or end your turn."
        ),
        (Unanswered::Wait, Some(when), _) => format!(
            "Shown to the person. Do nothing they have not picked. Unless they answer or write to \
             you first, Sonny reminds you at {when}. Meanwhile do work that does not depend on the \
             answer, or end your turn."
        ),
        (_, _, Some(error)) => format!(
            "Shown to the person, but Sonny could not set its deadline ({error}), so it waits for \
             their answer: do nothing they have not picked. Meanwhile do work that does not depend \
             on the answer, or end your turn."
        ),
        _ => "Shown to the person, whose setting has every ask wait for their answer: do nothing \
             they have not picked. Meanwhile do work that does not depend on the answer, or end \
             your turn."
            .to_owned(),
    };
    Ok(
        json!({"id": ask.id, "if_unanswered": plan.name(), "deadline_s": deadline,
        "deadline": when, "deadline_error": unset, "next": next})
        .to_string(),
    )
}

/// What the agent may do meanwhile, given the minutes its deadline waits:
/// with deadlines off (0), every ask waits for the person, `go_now` too.
fn allowed(plan: Unanswered, minutes: u64) -> Unanswered {
    if minutes == 0 { Unanswered::Wait } else { plan }
}

/// The one-off trigger that wakes `bot` at `deadline` about its ask, unless
/// `sonny ask open` finds it settled by then.
fn set_deadline(
    ask: &Ask,
    plan: Unanswered,
    label: &str,
    deadline: i64,
    bot: &str,
    turn: i64,
    call: &str,
) -> Result<(), String> {
    let sonny = crate::skills::sonny_path().ok_or("no path to this Sonny")?;
    let quote = |text: &str| format!("'{}'", text.replace('\'', r"'\''"));
    let gate = format!(
        "{} ask open {} {turn} {}",
        quote(&sonny.to_string_lossy()),
        quote(bot),
        quote(call)
    );
    let at = crate::trigger::local(deadline);
    let at = format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        at.year, at.month, at.day, at.hour, at.minute
    );
    // Sonny wrote its gate: it runs as Sonny's own.
    crate::trigger::add_sonnys(
        &trigger_name(bot, turn, call),
        &at,
        &gate,
        &fire(&ask.question, plan, label),
    )
    .map(|_| ())
}

/// The whole minute `minutes` from `now`, never before it, as a one-off
/// trigger keeps time.
fn deadline(minutes: u64, now: i64) -> i64 {
    let at = now + minutes as i64 * 60;
    at + (60 - at.rem_euclid(60)) % 60
}

/// The trigger an ask's deadline is, by its agent, turn and call (call IDs
/// repeat across turns): a name the person reads in `/background` beside
/// the agent's own.
fn trigger_name(bot: &str, turn: i64, call: &str) -> String {
    let tail: String = call
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let bot: String = bot
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                true => c,
                false => '-',
            },
        )
        .take(100)
        .collect();
    format!("{bot}.ask-{turn}-{tail}")
}

/// What the deadline's fire tells the agent: Sonny's words, never the
/// person's, so it allows nothing that needs theirs.
fn fire(question: &str, plan: Unanswered, label: &str) -> String {
    let first = question.lines().next().unwrap_or_default();
    let question = crate::transcript::cut(first, QUOTED);
    let label = crate::transcript::cut(label.lines().next().unwrap_or_default(), QUOTED);
    let tail = "This is Sonny, not the person: it is no answer of theirs, and it allows nothing \
        that needs their own words.";
    match plan {
        Unanswered::Later => format!(
            "Nobody answered your question \"{question}\" in time, so go on now with \"{label}\", \
             the option you recommended, as you said you would. Keep it undoable, and if they pick \
             another later, undo it and follow their pick. {tail}"
        ),
        _ => format!(
            "Your question \"{question}\" still has no answer, and you said it waits for the \
             person, so do not go ahead with it. Go on with what does not depend on it, and say in \
             one line what it holds up, so they see it again. {tail}"
        ),
    }
}

/// Where an ask stands, read from its agent's history newest first, one
/// entry at a time, back to the call that asked it, as [`open`] reads it:
/// answered by the person, closed by the agent, set aside by words of the
/// person's own after it, or open.
#[derive(Default)]
struct Standing {
    /// The person's answers after the call, by ask, newest first.
    answers: Vec<(String, String)>,
    /// The agent's closes after it that were taken, by ask, newest first.
    closes: Vec<(String, State)>,
    /// The calls taken whose call is not read yet, by turn and call: a
    /// close counts once its result says it was taken, as one still
    /// running may yet fail. A result comes after its call, so each is
    /// let go once its call is read.
    taken: Vec<(i64, Value)>,
    /// The one ask read for, when only its answers and closes are kept,
    /// so a long history costs no more than that ask's.
    wanted: Option<String>,
    /// The person wrote words of their own after it.
    moved_on: bool,
    /// A call not to count, by turn and call: the close being answered.
    skip: Option<(i64, Value)>,
}

impl Standing {
    /// Reads `entry`, of turn `at`, newest first, for ask `call` of turn
    /// `turn`: once the call is reached, the ask it put, if it put one,
    /// and where it stands. A provider's call IDs are unique only within
    /// a turn, so another turn's call of the same ID is not the ask, and
    /// the reading goes on past it.
    fn entry(
        &mut self,
        at: i64,
        entry: &Value,
        turn: i64,
        call: &str,
    ) -> Option<Option<(Ask, State)>> {
        let items = self.read(at, entry);
        if at != turn {
            return None;
        }
        let (made, _) = (items.iter())
            .find(|(item, _)| item["type"] == "function_call" && item["call_id"] == call)?;
        let asked = id(turn, call);
        let ask = (made["arguments"].as_str())
            .and_then(|a| serde_json::from_str::<Value>(a).ok())
            .and_then(|arguments| self::call(&asked, &arguments));
        Some(ask.map(|ask| {
            let state = self.state(&asked);
            (ask, state)
        }))
    }

    /// Takes in what `entry`, of turn `at`, says of the asks before it:
    /// answers, closes, results and the person's own words. Its calls,
    /// each with whether it was taken.
    fn read(&mut self, at: i64, entry: &Value) -> Vec<(Value, bool)> {
        let items = crate::responses(&entry["item"]);
        let mut read = Vec::with_capacity(items.len());
        let wanted = |of: &str| self.wanted.as_deref().is_none_or(|wanted| wanted == of);
        // An entry's results first, as they come after its calls.
        self.taken.extend(
            (items.iter())
                .filter(|item| {
                    item["type"] == "function_call_output"
                        && item["failed"] != true
                        && !crate::reactions::refused(item)
                })
                .map(|item| (at, item["call_id"].clone())),
        );
        let mut closes = Vec::new();
        for item in items.iter().filter(|item| item["type"] == "function_call") {
            let made = (at, item["call_id"].clone());
            let was_taken = (self.taken.iter().position(|taken| *taken == made))
                .map(|at| self.taken.swap_remove(at))
                .is_some();
            read.push((item.clone(), was_taken));
            if let Some((closed, state)) = closed_by(item)
                && self.skip.as_ref() != Some(&made)
                && was_taken
                && wanted(&closed)
            {
                closes.push((closed, state));
            }
        }
        self.closes.extend(closes);
        if crate::reactions::said_by_person(entry).is_some() {
            let text = crate::text(&entry["item"]);
            match (answer_of(&text, None), split(&text)) {
                (Some(answered), Some((_, answer))) => {
                    if self
                        .wanted
                        .as_deref()
                        .is_none_or(|wanted| wanted == answered)
                    {
                        self.answers
                            .push((answered.to_owned(), answer.trim().to_owned()));
                    }
                }
                _ if moves_on(&text, entry["from"].as_str()) => self.moved_on = true,
                _ => {}
            }
        }
        read
    }

    /// Where ask `asked` stands by what was read after it. The person's
    /// first answer counts, as the daemon takes only the first, over any
    /// close; then the agent's first close that was taken.
    fn state(&self, asked: &str) -> State {
        if let Some((_, answer)) = (self.answers.iter().rev()).find(|(of, _)| of == asked) {
            return State::Answered(answer.clone());
        }
        let closed = (self.closes.iter().rev()).find(|(of, _)| of == asked);
        match closed {
            Some((_, state)) => state.clone(),
            None if self.moved_on => State::Aside,
            None => State::Open,
        }
    }
}

/// Every question one agent's history put, read newest first as
/// [`Standing`] reads one: each where it stands by what came after it.
#[derive(Default)]
pub struct Asked(Standing);

impl Asked {
    /// The asks `entry`, of turn `at`, put, each with where it stands; an
    /// ask the tool refused, or has not taken yet, put none.
    pub fn entry(&mut self, at: i64, entry: &Value) -> Vec<(Ask, State)> {
        let items = self.0.read(at, entry);
        (items.iter())
            .filter(|(item, taken)| *taken && item["name"] == TOOL)
            .filter_map(|(item, _)| {
                let asked = id(at, item["call_id"].as_str()?);
                let arguments = serde_json::from_str::<Value>(item["arguments"].as_str()?).ok()?;
                let ask = self::call(&asked, &arguments)?;
                Some((ask, self.0.state(&asked)))
            })
            .collect()
    }
    /// Whether the person wrote words of their own in what was read: the
    /// questions before those are set aside.
    pub fn moved_on(&self) -> bool {
        self.0.moved_on
    }
}

/// Whether the ask `asked` by call `call` is still open, read as
/// [`Standing`] reads it, stopping at the person's own words past it.
/// `None` while the entries read have not reached the call.
#[derive(Default)]
struct Reading(Standing);

impl Reading {
    fn entry(&mut self, at: i64, entry: &Value, turn: i64, call: &str) -> Option<bool> {
        let found = self.0.entry(at, entry, turn, call);
        if self.0.moved_on {
            return Some(false);
        }
        found.map(|found| found.is_some_and(|(_, state)| state == State::Open))
    }
}

/// Where ask `call` of `bot`'s turn `turn` stands, read from its history:
/// none when that call put no question. A close made as call `skip`, the
/// one being answered, is not counted.
pub async fn standing(
    client: &agent_client::Client,
    bot: &str,
    turn: i64,
    call: &str,
    skip: Option<(i64, &str)>,
) -> agent_client::Result<Option<(Ask, State)>> {
    let mut reading = Standing {
        skip: skip.map(|(turn, call)| (turn, json!(call))),
        wanted: Some(id(turn, call)),
        ..Standing::default()
    };
    let mut found = None;
    crate::said::walk(client, bot, turn, |at, entry| {
        found = reading.entry(at, entry, turn, call);
        found.is_some()
    })
    .await?;
    Ok(found.flatten())
}

/// Sonny's answer to a `close_ask` call by `bot`: the ask closed, once it
/// is the agent's and nobody answered it, its deadline let go. The
/// person's own answer stands: a question they answered is not closed.
pub async fn served_close(
    arguments: &Value,
    bot: &str,
    this: Option<(i64, &str)>,
) -> Result<String, String> {
    let (asked, state) = closing(arguments).ok_or(
        "invalid_arguments: the ask's ID as ask gave it (ask:TURN/CALL), and outcome answered or \
         withdrawn",
    )?;
    if matches!(&state, State::Recorded { note, .. } | State::Withdrawn(note) if note.is_empty()) {
        return Err("invalid_arguments: a line in note for the card".into());
    }
    let (turn, call) = (asked.strip_prefix("ask:"))
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(turn, call)| Some((turn.parse::<i64>().ok()?, call.to_owned())))
        .ok_or("invalid_arguments: the ask's ID as ask gave it (ask:TURN/CALL)")?;
    let daemon = crate::trigger::Daemon::current()?;
    let socket = daemon.socket()?;
    let client = crate::trigger::connect(&socket, &daemon).await?;
    let found = standing(&client, bot, turn, &call, this)
        .await
        .map_err(|e| e.to_string())?;
    let Some((ask, now)) = found else {
        return Err(format!("ask_not_found: {asked} is no question of yours"));
    };
    match now {
        State::Answered(answer) => {
            return Err(format!(
                "answered: the person answered it themselves (\"{}\"), and their answer stands",
                crate::transcript::cut(answer.lines().next().unwrap_or(""), QUOTED)
            ));
        }
        State::Recorded { .. } | State::Withdrawn(_) => {
            return Err(format!("closed: {asked} is closed already"));
        }
        State::Open | State::Aside => {}
    }
    if let State::Recorded {
        option: Some(at), ..
    } = state
        && at >= ask.options.len()
    {
        return Err(format!(
            "invalid_arguments: option is a number from 1 to {}",
            ask.options.len()
        ));
    }
    // Its deadline goes with it: nothing to fire, nothing left on disk.
    let name = trigger_name(bot, turn, &call);
    let _ = tokio::task::spawn_blocking(move || {
        crate::trigger::tool(&json!({"remove": name}), &crate::trigger::Caller::default())
    })
    .await;
    let outcome = match state {
        State::Withdrawn(_) => "withdrawn",
        _ => "answered",
    };
    Ok(json!({"closed": asked, "outcome": outcome,
        "next": "Closed: its card shows the outcome and your note. A pick of theirs on it later \
            still answers it; follow that pick if it comes."})
    .to_string())
}

/// Whether the ask is still open, read from the daemon this shell reaches.
pub async fn still_open(bot: &str, turn: i64, call: &str) -> Result<bool, String> {
    let daemon = crate::trigger::Daemon::current()?;
    let socket = daemon.socket()?;
    let client = crate::trigger::connect(&socket, &daemon).await?;
    let wanted = Standing {
        wanted: Some(id(turn, call)),
        ..Standing::default()
    };
    let (mut reading, mut found) = (Reading(wanted), None);
    crate::said::walk(&client, bot, turn, |at, entry| {
        found = reading.entry(at, entry, turn, call);
        found.is_some()
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(found == Some(true))
}

/// `sonny ask open BOT TURN CALL`, an ask's trigger's `--if`: exit 0
/// while the ask is open, 1 once it is settled, 2 when it cannot be read.
pub fn cli(args: &[String]) -> i32 {
    let [verb, bot, turn, call] = args else {
        eprintln!("usage: sonny ask open BOT TURN CALL");
        return 2;
    };
    let Ok(turn) = turn.parse::<i64>() else {
        eprintln!("usage: sonny ask open BOT TURN CALL");
        return 2;
    };
    if verb != "open" {
        eprintln!("usage: sonny ask open BOT TURN CALL");
        return 2;
    }
    let open =
        crate::trigger::runtime().and_then(|runtime| runtime.block_on(still_open(bot, turn, call)));
    match open {
        Ok(true) => 0,
        Ok(false) => 1,
        // Not a no: the deadline's fire fails and says why.
        Err(error) => {
            eprintln!("{error}");
            2
        }
    }
}

/// Which of the open questions `at` is, from 0, when there are several.
fn place(at: usize, of: usize) -> String {
    match of {
        0 | 1 => String::new(),
        of => format!(" ({} of {of})", at + 1),
    }
}

/// The composer's hint while what is typed answers `ask`, the `at`th of
/// `of` open, cut to `width`: which question, muted, then its keys in
/// the one grammar.
pub fn hint(ask: &Ask, at: usize, of: usize, width: usize) -> Vec<Span<'static>> {
    let mut keys = match ask.options.len() {
        0 => Vec::new(),
        1 => vec!["1 pick".to_owned()],
        n => vec![format!("1–{n} pick")],
    };
    if of > 1 {
        keys.push("↑↓ switch".into());
    }
    keys.push("esc write instead".into());
    let keys = keys.join(" · ");
    // Words answer it too: typed, or for a step of the person's, how it
    // went.
    let words = match ask.options.len() {
        0 if ask.command.is_some() => " say how it went ·",
        0 => " type your answer ·",
        _ => " type or",
    };
    let place = place(at, of);
    // A task's question, pinned in its lead's window, by whose it is.
    let whose = (ask.task.as_ref()).map_or(String::new(), |task| format!(" from {}", task.name));
    let room = width.saturating_sub(
        "Answer “”: ".chars().count()
            + whose.chars().count()
            + place.chars().count()
            + words.chars().count()
            + keys.chars().count(),
    );
    let question = crate::transcript::cut(&plain(&ask.question), room.max(8));
    let mut spans = vec![Span::raw(format!("Answer “{question}”{whose}{place}:{words} ")).dim()];
    spans.extend(crate::hint::spans(&keys));
    spans
}

/// The question as it reads on its card, without its Markdown marks.
fn plain(question: &str) -> String {
    crate::transcript::inline(question.lines().next().unwrap_or(""), Style::default())
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// The row over what is typed while it answers `ask`, the `at`th of
/// `of` open: the row the answer has over it once sent, so what is
/// written reads as it will be read.
pub fn answering_typed(ask: &Ask, at: usize, of: usize, width: usize) -> Line<'static> {
    answering_row(&ask.question, &place(at, of), width)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `asks` without the one `text` answers, as a window lets go of it
    /// ([`answer_of`]); whether it was there. A task's question is
    /// answered in the task's conversation, not here.
    fn close(asks: &mut Vec<Ask>, text: &str, from: Option<&str>) -> bool {
        let at = answer_of(text, from)
            .and_then(|call| (asks.iter()).position(|ask| ask.id == call && ask.task.is_none()));
        at.map(|at| asks.remove(at)).is_some()
    }

    fn keep_api() -> Value {
        json!({
            "question": " Keep the old API? ",
            "why": "The wire format changes, and 0.3 clients break.",
            "options": [
                {"label": "Keep it", "explanation": "Old clients work; a shim stays a release."},
                {"label": " ", "explanation": "nothing"},
                {"label": "Drop it", "explanation": "A smaller daemon; 0.3 users upgrade both."}
            ],
            "recommended": 2,
            "reason": "Nobody runs 0.3 outside this machine."
        })
    }

    #[test]
    fn a_call_puts_its_question_and_an_answer_closes_it() {
        let ask = call(&id(1, "c1"), &keep_api()).unwrap();
        assert_eq!(ask.question, "Keep the old API?");
        assert_eq!(
            ask.options
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["Keep it", "Drop it"]
        );
        assert_eq!(ask.recommended, Some(1));
        assert_eq!(call(&id(1, "c2"), &json!({"question": " "})), None);
        // A recommendation past the options recommends none.
        let past = call(
            &id(1, "c2"),
            &json!({"question": "Tag it?", "recommended": 3}),
        )
        .unwrap();
        assert_eq!(past.recommended, None);
        let mut open = vec![ask.clone(), past];
        close(&mut open, "how is it going?", None);
        assert_eq!(open.len(), 2);
        // The same question asked again is another decision: its answer
        // closes it alone, whichever came first.
        let again = call(&id(1, "c3"), &keep_api()).unwrap();
        open.insert(0, again.clone());
        close(&mut open, &answer(&ask, "2"), None);
        assert_eq!(
            open.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["ask:1/c3", "ask:1/c2"]
        );
        close(&mut open, &answer(&again, "1"), None);
        // A question with a blank line in it is still closed by its answer.
        let long = call(
            &id(1, "c4"),
            &json!({"question": "Ship it?\n\n  Tests pass.  "}),
        )
        .unwrap();
        assert_eq!(long.question, "Ship it?\n  Tests pass.");
        open.push(long.clone());
        close(&mut open, &answer(&long, "yes"), None);
        assert_eq!(open.len(), 1);
    }

    #[test]
    fn a_number_answers_with_its_option() {
        let ask = call("c1", &keep_api()).unwrap();
        assert_eq!(
            answer(&ask, "2"),
            "You asked (c1): Keep the old API?\n\nDrop it"
        );
        assert_eq!(
            answer(&ask, "3"),
            "You asked (c1): Keep the old API?\n\n3",
            "no third option: the digit is the answer"
        );
        assert_eq!(
            answer(&ask, "Keep it for one release"),
            "You asked (c1): Keep the old API?\n\nKeep it for one release"
        );
    }

    #[test]
    fn the_decision_reads_as_question_why_options_and_recommendation() {
        let ask = call("c1", &keep_api()).unwrap();
        let Card { rows, options, .. } = card(&ask, &State::Open, false, 60);
        let shown: Vec<String> = rows.iter().map(|row| row.to_string()).collect();
        assert_eq!(
            shown,
            [
                "? Keep the old API?",
                "┃ The wire format changes, and 0.3 clients break.",
                "┃ ",
                "┃ 1  Keep it",
                "┃    Old clients work; a shim stays a release.",
                "┃ 2  Drop it · recommended",
                "┃    A smaller daemon; 0.3 users upgrade both.",
                "┃ ",
                "┃ Why 2: Nobody runs 0.3 outside this machine.",
            ]
        );
        // Where a click picks each option.
        assert_eq!(options, [3, 5]);
        // Narrow, each part wraps under its own indent.
        let narrow: Vec<String> = card(&ask, &State::Open, false, 30)
            .rows
            .iter()
            .map(|row| row.to_string().trim_end().to_owned())
            .collect();
        let at = narrow.iter().position(|row| row == "┃ 1  Keep it").unwrap();
        assert_eq!(narrow[at + 1], "┃    Old clients work; a shim");
        assert_eq!(narrow[at + 2], "┃    stays a release.");
        // A yellow ? starts it, and a muted bar goes down the rest of it,
        // dim rows and bold ones alike.
        for (at, row) in rows.iter().enumerate() {
            let mark = row.spans.iter().find(|span| !span.content.is_empty());
            let mark = mark.expect("a row drawn");
            match at {
                0 => assert_eq!(
                    (mark.content.as_ref(), mark.style.fg),
                    (GLYPH, Some(Color::Yellow))
                ),
                _ => assert_eq!(
                    (
                        mark.content.as_ref(),
                        mark.style.fg,
                        mark.style.add_modifier
                    ),
                    (BAR, None, ratatui::style::Modifier::DIM),
                    "{row}"
                ),
            }
        }
    }

    #[test]
    fn a_yes_says_what_it_covers_on_the_card_and_in_the_answer() {
        let mut publish = keep_api();
        publish["covers"] =
            json!({"action": "publish", "target": "lydakis/agent#12", "revision": " 3f2a9c1 "});
        let ask = call(&id(7, "c1"), &publish).unwrap();
        let covers = ask.covers.clone().unwrap();
        assert_eq!(covers.line(), "publish · lydakis/agent#12 · at 3f2a9c1");
        // Under the why, muted, as the why is.
        let rows = card(&ask, &State::Open, false, 60).rows;
        assert_eq!(
            rows[2].to_string(),
            "┃ Covers: publish · lydakis/agent#12 · at 3f2a9c1"
        );
        let text = (rows[2].spans.iter())
            .find(|span| span.content.contains("Covers"))
            .unwrap();
        let dim = text.style.patch(rows[2].style).add_modifier;
        assert!(dim.contains(ratatui::style::Modifier::DIM), "{dim:?}");
        // The answer repeats it under the question, and still names its ask,
        // its question and its pick.
        let sent = answer(&ask, "2");
        assert_eq!(
            sent,
            "You asked (ask:7/c1): Keep the old API?\n\
             Covers: publish · lydakis/agent#12 · at 3f2a9c1\n\nDrop it"
        );
        assert_eq!(answered(&sent), Some("ask:7/c1"));
        assert_eq!(split(&sent), Some(("Keep the old API?", "Drop it")));
        // A target with no revision says none; an action or a target
        // missing covers nothing, so the card and answer say nothing.
        publish["covers"] = json!({"action": "send", "target": "the release notes"});
        let ask = call(&id(7, "c1"), &publish).unwrap();
        assert_eq!(ask.covers.unwrap().line(), "send · the release notes");
        publish["covers"] = json!({"action": "merge", "target": " "});
        let ask = call(&id(7, "c1"), &publish).unwrap();
        assert_eq!(ask.covers, None);
        assert_eq!(
            answer(&ask, "1"),
            "You asked (ask:7/c1): Keep the old API?\n\nKeep it"
        );
        // The tool asks for it, and says what a yes covers.
        let tool = tool();
        let covers = &tool["parameters"]["properties"]["covers"];
        assert_eq!(covers["required"], json!(["action", "target"]));
        assert!(
            (tool["description"].as_str().unwrap())
                .contains("a new target or revision needs a new ask")
        );
    }

    #[test]
    fn what_the_agent_wrote_in_a_decision_reads_as_markdown() {
        let ask = call(
            "c1",
            &json!({
                "question": "Ship **0.5** with `--fast` on?",
                "options": [{"label": "Yes", "explanation": "Runs $n^2$ checks, see [the plan](https://x.dev)."}],
            }),
        )
        .unwrap();
        let shown: Vec<String> = card(&ask, &State::Open, false, 80)
            .rows
            .iter()
            .map(|row| row.to_string())
            .collect();
        assert_eq!(shown[0], "? Ship 0.5 with --fast on?");
        assert_eq!(
            shown[3],
            "┃    Runs n² checks, see the plan (https://x.dev)."
        );
        // The question stays bold, past what its words make bold.
        let question = &card(&ask, &State::Open, false, 80).rows[0];
        let words = question
            .spans
            .iter()
            .skip_while(|span| span.content != GLYPH);
        assert!(words.skip(1).all(|span| {
            span.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        }));
        // The composer names it without its marks.
        assert!(text(&hint(&ask, 0, 1, 80)).starts_with("Answer “Ship 0.5 with --fast on?”"));
    }

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn the_hint_names_the_question_which_of_those_open_and_its_keys() {
        let ask = call("c1", &keep_api()).unwrap();
        assert_eq!(
            text(&hint(&ask, 0, 1, 80)),
            "Answer “Keep the old API?”: type or 1–2 pick · esc write instead"
        );
        assert_eq!(
            text(&hint(&ask, 1, 2, 100)),
            "Answer “Keep the old API?” (2 of 2): type or 1–2 pick · ↑↓ switch · esc write instead"
        );
        assert!(text(&hint(&ask, 0, 1, 50)).contains('…'));
        // The question muted, the keys in the one grammar.
        let spans = hint(&ask, 0, 2, 80);
        let key = |text: &str| spans.iter().find(|span| span.content == text).unwrap();
        for text in ["1–2", "↑↓", "esc"] {
            let dim = key(text).style.add_modifier;
            assert!(!dim.contains(ratatui::style::Modifier::DIM), "{text}");
        }
        assert!(
            spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
        // No options: the answer is typed.
        let open = call("c2", &json!({"question": "Why?"})).unwrap();
        assert_eq!(
            text(&hint(&open, 0, 1, 80)),
            "Answer “Why?”: type your answer · esc write instead"
        );
        // Over what is typed, the row the sent answer will have.
        assert_eq!(
            answering_typed(&ask, 0, 2, 80).to_string(),
            "  answering “Keep the old API?” (1 of 2)"
        );
        assert_eq!(
            answering_typed(&ask, 0, 1, 80).to_string(),
            answering(&ask.question, 80).to_string()
        );
    }

    #[test]
    fn the_persons_own_words_move_on_and_nothing_else_does() {
        assert!(moves_on("Meanwhile, why so slow?", None));
        assert!(!moves_on("You asked (ask:1/a1): Keep it?\n\nYes", None));
        assert!(!moves_on("Done: tests pass", Some("tally.halves")));
        assert!(!moves_on(&crate::reactions::message("👍", None), None));
    }

    #[test]
    fn a_question_is_open_until_its_answer_lands() {
        let asking = |call: &str, turn: i64, question: &str| {
            json!({"type": "function_call", "name": TOOL, "call_id": call, "turn": turn,
                "arguments": json!({"question": question,
                    "options": [{"label": "Yes", "explanation": "e"}]}).to_string()})
        };
        let asked = asking("a1", 3, "Keep it?");
        let reply =
            json!({"role": "assistant", "content": [{"type": "output_text", "text": "Asked."}]});
        let output = json!({"type": "function_call_output", "call_id": "a1", "output": "Asked."});
        let person =
            |text: &str| json!({"role": "user", "content": [{"type": "input_text", "text": text}]});
        let answered = |call: &str, turn: i64, question: &str| {
            let ask = call_of(call, turn, question);
            person(&answer(&ask, "1"))
        };
        // Whole, under the ID its answer names.
        let whole = super::open(&[reply.clone(), asked.clone()]).unwrap();
        assert_eq!((whole.id.as_str(), whole.options.len()), ("ask:3/a1", 1));
        // As Sonny took it, by its own call's result: what the agent does
        // meanwhile, and when. Another turn's result under the same call
        // is another ask's.
        let took = |turn: i64| {
            json!({"type": "function_call_output", "call_id": "a1", "turn": turn,
                "output": json!({"if_unanswered": "go_later", "deadline_s": 1700}).to_string()})
        };
        let taken = super::open(&[reply.clone(), took(3), asked.clone()]).unwrap();
        assert_eq!(
            (taken.unanswered, taken.deadline),
            (Some(Unanswered::Later), Some(1700))
        );
        let other = super::open(&[reply.clone(), took(7), asked.clone()]).unwrap();
        assert_eq!(other.deadline, None);
        let open = |items: &[Value]| super::open(items).map(|ask| ask.question);
        // Newest first.
        assert_eq!(
            open(&[reply.clone(), output.clone(), asked.clone()]),
            Some("Keep it?".into())
        );
        // A reaction answers nothing.
        let thumbs = crate::reactions::message("👍", None);
        assert_eq!(
            open(&[person(&thumbs), reply.clone(), asked.clone()]),
            Some("Keep it?".into())
        );
        // Words of the person's own after it set it aside: the
        // conversation went on past it. Before it, they do not.
        assert_eq!(
            open(&[person("Meanwhile, why?"), reply.clone(), asked.clone()]),
            None
        );
        assert_eq!(
            open(&[reply.clone(), asked.clone(), person("Plan the release")]),
            Some("Keep it?".into())
        );
        assert_eq!(
            open(&[reply.clone(), answered("a1", 3, "Keep it?"), asked.clone()]),
            None
        );
        assert_eq!(open(&[reply, output]), None);
        // Two asked, one answered: the other stays open.
        let other = asking("a2", 3, "Ship it?");
        assert_eq!(
            open(&[answered("a1", 3, "Keep it?"), other, asked.clone()]),
            Some("Ship it?".into())
        );
        // Two asking the same question are two decisions: answering the
        // later leaves the earlier open, whatever the words.
        let again = asking("a3", 3, "Keep it?");
        assert_eq!(
            open(&[answered("a3", 3, "Keep it?"), again, asked.clone()]),
            Some("Keep it?".into())
        );
        // A provider's call IDs are unique only within a turn: an ask of
        // another turn with the same call is another decision.
        let later = asking("a1", 7, "Tag it?");
        assert_eq!(
            open(&[answered("a1", 7, "Tag it?"), later.clone(), asked.clone()]),
            Some("Keep it?".into())
        );
        assert_eq!(
            open(&[answered("a1", 3, "Keep it?"), later, asked.clone()]),
            Some("Tag it?".into())
        );
        // The ID names no agent: a fork reads its source's turns as they
        // were, so an answer its source got before the fork closes the
        // same ask in the fork's history.
        assert_eq!(id(3, "a1"), "ask:3/a1");
        assert_eq!((turn_of("ask:3/a1"), turn_of("a1")), (Some(3), None));
        let unknown = json!({"type": "function_call_output", "call_id": "a1", "turn": 3,
            "output": json!({"error": "unknown_tool"}).to_string()});
        assert_eq!(open(&[unknown, asked]), None);
    }

    /// Ask `call` of turn `turn`, asking `question`.
    fn call_of(call: &str, turn: i64, question: &str) -> Ask {
        self::call(
            &id(turn, call),
            &json!({"question": question, "options": [{"label": "Yes", "explanation": "e"}]}),
        )
        .unwrap()
    }

    #[test]
    fn an_answer_names_its_ask_whole_and_one_sent_before_reads_as_words() {
        let ask = call_of("toolu_1", 12, "Publish it?");
        let sent = answer(&ask, "1");
        assert_eq!(sent, "You asked (ask:12/toolu_1): Publish it?\n\nYes");
        assert_eq!(answer_of(&sent, None), Some("ask:12/toolu_1"));
        assert_eq!(split(&sent), Some(("Publish it?", "Yes")));
        // One sent naming the call alone could be another turn's: it
        // answers none, and is drawn as the answer it was.
        let old = "You asked (toolu_1): Publish it?\n\nYes";
        assert_eq!(answer_of(old, None), None);
        assert!(moves_on(old, None));
        assert_eq!(split(old), Some(("Publish it?", "Yes")));
        // Every window sends one ask's answer under the same request,
        // and another ask's under another, within the daemon's names.
        let request = request_id(&ask.id, 0);
        assert_eq!(request, request_id("ask:12/toolu_1", 0));
        assert_ne!(request, request_id("ask:13/toolu_1", 0));
        // One taken back unread goes again as the next attempt.
        let again = request_id(&ask.id, ATTEMPTS);
        assert_ne!(request_id(&ask.id, 1), request);
        for request in [&request, &again] {
            assert!(request.len() <= 128);
            assert!((request.bytes()).all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)));
        }
    }

    /// The card's rows as a screen draws them at `width`, laid at `made`.
    fn drawn(ask: &Ask, made: usize, width: u16) -> Vec<String> {
        use ratatui::{buffer::Buffer, layout::Rect};
        let card = card(ask, &State::Open, false, made);
        let mut screen = crate::screen::Screen::default();
        let marks = (card.hangs.into_iter().enumerate())
            .map(|(row, hang)| (row, crate::screen::Mark::Hangs(hang)))
            .collect();
        screen.push_marked(card.rows, marks);
        let area = Rect::new(0, 0, width, 60);
        let mut buf = Buffer::empty(area);
        screen.render(area, &mut buf, |_| None);
        (0..area.height)
            .map(|y| {
                // A quote's bar, a tinted blank, read as its mark.
                let row: String = (0..width)
                    .map(|x| match buf[(x, y)].bg == crate::look::QUOTE {
                        true => "▌",
                        false => buf[(x, y)].symbol(),
                    })
                    .collect();
                row.trim_end().to_owned()
            })
            .filter(|row| !row.is_empty())
            .collect()
    }

    #[test]
    fn the_bar_goes_on_down_a_card_drawn_narrower_than_it_was_laid() {
        let ask = call(
            "c1",
            &json!({
                "question": "Which store migration should 0.8 ship with, the online one or the offline one?",
                "why": "The choice decides how long the daemon is down.\n\n\n- **Online**: copies rows in batches while the daemon runs.\n- **Offline**: rewrites the table in one pass.\n\n> Both were tried on a copy of a real store.",
                "options": [
                    {"label": "Online migration, batched while the daemon keeps serving", "explanation": "No downtime, but the upgrade takes about forty minutes."},
                    {"label": "Offline rewrite", "explanation": "About three minutes."}
                ],
                "recommended": 2,
                "reason": "Three minutes of pause is less than forty of slow searches."
            }),
        )
        .unwrap();
        // Laid wide, as the margin opening after it narrows it.
        let rows = drawn(&ask, 100, 34);
        assert!(rows[0].starts_with("? Which"));
        for row in &rows[1..] {
            assert!(row.starts_with('┃'), "{row}\n{}", rows.join("\n"));
        }
        // Each row goes on under its own text: a list item past its
        // marker, an option past its number, a quote under its bar.
        let at = |text: &str| rows.iter().position(|row| row.contains(text)).unwrap();
        assert!(rows[at("- Online")].starts_with("┃ - Online"));
        assert!(rows[at("- Online") + 1].starts_with("┃   "));
        assert!(rows[at("1  Online")].starts_with("┃ 1  Online"));
        assert!(rows[at("1  Online") + 1].starts_with("┃    "));
        assert!(rows[at("▌ Both")].starts_with("┃ ▌ Both"));
        assert!(
            rows[at("▌ Both") + 1].starts_with("┃ ▌ "),
            "{}",
            rows.join("\n")
        );
        // Paragraphs stay apart, by one row however many the agent left.
        assert_eq!(rows[at("- Online") - 1], "┃");
        assert_ne!(rows[at("- Online") - 2], "┃");
        // Laid at the width it is drawn at, it reads the same.
        assert_eq!(drawn(&ask, 34, 34), rows);
    }

    fn asked(plan: &str) -> Value {
        let mut arguments = keep_api();
        arguments["if_unanswered"] = json!(plan);
        arguments
    }

    fn closing_call(id: &str, outcome: &str, option: Option<u64>, note: &str) -> Value {
        let mut arguments = json!({"id": id, "outcome": outcome, "note": note});
        if let Some(option) = option {
            arguments["option"] = json!(option);
        }
        json!({"type": "function_call", "name": CLOSE, "call_id": "k1",
            "arguments": arguments.to_string()})
    }

    #[test]
    fn a_close_names_its_ask_whole_and_what_it_leaves() {
        let tool = close_tool();
        assert_eq!(tool["answered_by"], crate::served::TAG);
        assert_eq!(tool["name"], CLOSE);
        let read = |item: Value| closed_by(&item);
        assert_eq!(
            read(closing_call(
                "ask:2/a1",
                "answered",
                Some(2),
                "drop it\nnobody uses 0.3"
            )),
            Some((
                "ask:2/a1".into(),
                State::Recorded {
                    option: Some(1),
                    note: "drop it nobody uses 0.3".into()
                }
            ))
        );
        assert_eq!(
            read(closing_call(
                "ask:2/a1",
                "withdrawn",
                None,
                "settled in #812"
            )),
            Some((
                "ask:2/a1".into(),
                State::Withdrawn("settled in #812".into())
            ))
        );
        // Only a whole ID, and one of the two outcomes.
        assert_eq!(read(closing_call("a1", "withdrawn", None, "x")), None);
        assert_eq!(read(closing_call("ask:2/a1", "maybe", None, "x")), None);
    }

    #[test]
    fn every_ask_in_a_history_reads_with_where_it_stands() {
        let entry = |node: i64, item: &Value| json!({"node": node, "item": item});
        let asking = |call: &str| {
            json!({"type": "function_call", "name": TOOL, "call_id": call,
                "arguments": asked("wait").to_string()})
        };
        let refused = json!({"type": "function_call_output", "call_id": "a3", "failed": true,
            "output": "invalid_arguments"});
        let ask = call(&id(2, "a1"), &asked("wait")).unwrap();
        let person = |text: &str| {
            json!({"node": 9, "from": null, "origin": null,
            "item": {"role": "user", "content": [{"type": "input_text", "text": text}]}})
        };
        let taken = |call: &str| {
            json!({"type": "function_call_output", "call_id": call,
                "output": json!({"id": call}).to_string()})
        };
        // Newest first: a fourth not taken yet and a refused third, which
        // put nothing yet; words past the second, an answer to the first.
        let entries = [
            (6, entry(11, &asking("a4"))),
            (5, entry(9, &refused)),
            (5, entry(8, &asking("a3"))),
            (4, person("something else")),
            (3, entry(5, &taken("a2"))),
            (3, entry(4, &asking("a2"))),
            (3, person(&answer(&ask, "yes"))),
            (2, entry(2, &taken("a1"))),
            (2, entry(1, &asking("a1"))),
        ];
        let mut reading = Asked::default();
        let found: Vec<(String, State)> = (entries.iter())
            .flat_map(|(at, entry)| reading.entry(*at, entry))
            .map(|(ask, state)| (ask.id, state))
            .collect();
        assert_eq!(
            found,
            [
                ("ask:3/a2".to_owned(), State::Aside),
                ("ask:2/a1".to_owned(), State::Answered("yes".into()))
            ]
        );
    }

    #[test]
    fn an_agents_close_settles_its_ask_for_every_reader() {
        let asking = json!({"type": "function_call", "name": TOOL, "call_id": "a1", "turn": 2,
            "arguments": asked("wait").to_string()});
        let mut closed = closing_call("ask:2/a1", "withdrawn", None, "no longer needed");
        closed["turn"] = json!(3);
        let taken = json!({"type": "function_call_output", "call_id": "k1", "turn": 3,
            "output": "{\"closed\":\"ask:2/a1\"}"});
        // The window's open questions and needs, once the close was taken:
        // one still running may yet fail.
        assert!(open(std::slice::from_ref(&asking)).is_some());
        assert!(open(&[closed.clone(), asking.clone()]).is_some());
        assert!(open(&[taken.clone(), closed.clone(), asking.clone()]).is_none());
        // Refused, it closed nothing.
        let refused = json!({"type": "function_call_output", "call_id": "k1", "turn": 3,
            "failed": true, "output": "closed already"});
        assert!(open(&[refused.clone(), closed.clone(), asking.clone()]).is_some());
        // The deadline, and where it stands.
        let entry = |node: i64, item: &Value| json!({"node": node, "item": item});
        let person = |text: &str| {
            json!({"node": 9, "from": null, "origin": null,
            "item": {"role": "user", "content": [{"type": "input_text", "text": text}]}})
        };
        let stands = |entries: &[(i64, Value)]| {
            let mut reading = Standing::default();
            entries
                .iter()
                .find_map(|(at, entry)| reading.entry(*at, entry, 2, "a1"))
                .flatten()
                .map(|(_, state)| state)
        };
        let open_one = (2, entry(1, &asking));
        assert_eq!(stands(std::slice::from_ref(&open_one)), Some(State::Open));
        let closing = (3, entry(5, &closed));
        let took = (3, entry(6, &taken));
        assert_eq!(
            stands(&[took.clone(), closing.clone(), open_one.clone()]),
            Some(State::Withdrawn("no longer needed".into()))
        );
        // Still running, it closed nothing yet.
        assert_eq!(
            stands(&[closing.clone(), open_one.clone()]),
            Some(State::Open)
        );
        // The close being answered reads its own call in the history,
        // and does not count it.
        let mut reading = Standing {
            skip: Some((3, json!("k1"))),
            ..Standing::default()
        };
        assert_eq!(
            [took.clone(), closing.clone(), open_one.clone()]
                .iter()
                .find_map(|(at, entry)| reading.entry(*at, entry, 2, "a1"))
                .flatten()
                .map(|(_, state)| state),
            Some(State::Open)
        );
        let mut reading = Reading::default();
        assert_eq!(
            [took.clone(), closing.clone(), open_one.clone()]
                .iter()
                .find_map(|(at, entry)| reading.entry(*at, entry, 2, "a1")),
            Some(false)
        );
        assert_eq!(
            stands(&[(3, entry(6, &refused)), closing.clone(), open_one.clone()]),
            Some(State::Open)
        );
        // The person's words past it set it aside; their answer stands
        // over any close.
        assert_eq!(
            stands(&[(4, person("hold on")), open_one.clone()]),
            Some(State::Aside)
        );
        let ask = call(&id(2, "a1"), &asked("wait")).unwrap();
        assert_eq!(
            stands(&[
                (4, person(&answer(&ask, "talk later"))),
                took.clone(),
                closing.clone(),
                open_one
            ]),
            Some(State::Answered("talk later".into()))
        );
        // Read for one ask, another's answers and closes are not kept,
        // and a result goes once its call is read.
        let other = call(&id(1, "z9"), &asked("wait")).unwrap();
        let mut reading = Standing {
            wanted: Some(id(2, "a1")),
            ..Standing::default()
        };
        let mut withdrawn_other = closing_call("ask:1/z9", "withdrawn", None, "n");
        withdrawn_other["turn"] = json!(3);
        for (at, entry) in [
            (4, person(&answer(&other, "no"))),
            (3, entry(7, &withdrawn_other)),
            (
                3,
                entry(
                    8,
                    &json!({"type": "function_call_output", "call_id": "k2",
                "turn": 3, "output": "ok"}),
                ),
            ),
            (3, entry(6, &refused)),
            closing,
        ] {
            reading.entry(at, &entry, 2, "a1");
        }
        assert!(reading.answers.is_empty() && reading.closes.is_empty());
        assert_eq!(reading.taken, [(3, json!("k2"))]);
    }

    #[test]
    fn a_card_says_where_its_question_stands_and_folds_once_settled() {
        let ask = call(&id(1, "c1"), &asked("wait")).unwrap();
        let shown = |state: &State, folded: bool| -> Vec<String> {
            card(&ask, state, folded, 80)
                .rows
                .iter()
                .map(|row| row.to_string())
                .collect()
        };
        // Open, as before: the yellow mark, what happens meanwhile.
        let open = card(&ask, &State::Open, true, 80);
        assert_eq!(open.rows[0].to_string(), "? Keep the old API?");
        let mark = |card: &Card| {
            let row = &card.rows[0];
            let span = row.spans.iter().find(|span| span.content == GLYPH).unwrap();
            row.style.patch(span.style)
        };
        assert_eq!(mark(&open).fg, Some(Color::Yellow));
        assert!(
            shown(&State::Open, true)
                .last()
                .unwrap()
                .contains("Waits for your pick")
        );
        assert_eq!(open.question, 0..1);
        // Set aside: whole, muted, a pick still answering it.
        let aside = card(&ask, &State::Aside, true, 80);
        assert!(
            mark(&aside)
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        );
        assert_ne!(mark(&aside).fg, Some(Color::Yellow));
        assert_eq!(aside.options.len(), 2);
        assert_eq!(
            shown(&State::Aside, true).last().unwrap(),
            "┃ Set aside: you wrote past it. A pick still answers it."
        );
        // Answered: two rows folded, whole once opened.
        let answered = State::Answered("Drop it\nmore".into());
        assert_eq!(
            shown(&answered, true),
            ["✓ Keep the old API?", "┃ Answered · Drop it …"]
        );
        let whole = shown(&answered, false);
        assert!(whole.len() > 2 && whole.last().unwrap() == "┃ Answered · Drop it …");
        assert!(!whole.iter().any(|row| row.contains("Waits for")));
        // Recorded by the agent, and withdrawn.
        let recorded = State::Recorded {
            option: Some(1),
            note: "said so in chat".into(),
        };
        assert_eq!(
            shown(&recorded, true)[1],
            "┃ Answered in your words · going with Drop it · said so in chat"
        );
        let withdrawn = State::Withdrawn("settled elsewhere".into());
        assert_eq!(
            shown(&withdrawn, true),
            ["– Keep the old API?", "┃ Withdrawn · settled elsewhere"]
        );
        assert!(withdrawn.takes_a_pick() && !answered.takes_a_pick());
    }

    #[test]
    fn the_tool_is_answered_by_sonny_and_asks_what_happens_meanwhile() {
        let tool = tool();
        assert_eq!(tool["answered_by"], crate::served::TAG);
        assert!(
            tool.get("result").is_none(),
            "a served tool has no fixed result"
        );
        let required = tool["parameters"]["required"].as_array().unwrap();
        assert!(required.contains(&json!("if_unanswered")));
        assert_eq!(
            tool["parameters"]["properties"]["if_unanswered"]["enum"],
            json!(["go_now", "go_later", "wait"])
        );
        // Anything outward-facing waits, and the wake is never the person.
        let description = tool["description"].as_str().unwrap();
        for words in [
            "in their name",
            "approves",
            "never their answer",
            "undo what you did",
        ] {
            assert!(description.contains(words), "{words}");
        }
    }

    #[test]
    fn what_the_agent_does_meanwhile_is_read_from_the_call_and_its_result() {
        assert_eq!(call("c", &keep_api()).unwrap().unanswered, None);
        assert_eq!(
            call("c", &asked("go_later")).unwrap().unanswered,
            Some(Unanswered::Later)
        );
        // Nothing recommended, nothing to go ahead with: it waits.
        let mut none = asked("go_now");
        none["recommended"] = json!(9);
        assert_eq!(call("c", &none).unwrap().unanswered, Some(Unanswered::Wait));
        // The result says how Sonny took it: deadlines off made it wait.
        let ask = call("c", &asked("go_later")).unwrap();
        let took = settled(ask.clone(), r#"{"if_unanswered":"wait","deadline_s":null}"#);
        assert_eq!(
            (took.unanswered, took.deadline),
            (Some(Unanswered::Wait), None)
        );
        let took = settled(
            ask.clone(),
            r#"{"if_unanswered":"go_later","deadline_s":1800000000}"#,
        );
        assert_eq!(took.deadline, Some(1_800_000_000));
        // A result from before, plain words, changes nothing.
        assert_eq!(settled(ask.clone(), "Asked."), ask);
    }

    #[test]
    fn the_card_says_what_happens_until_the_person_picks() {
        let now = 1_800_000_000;
        let words = |plan: &str, deadline: Option<i64>| {
            let mut ask = call("c", &asked(plan)).unwrap();
            ask.deadline = deadline;
            let said = meanwhile(&ask, now).unwrap();
            said.iter()
                .map(|(text, _)| text.as_str())
                .collect::<String>()
        };
        assert_eq!(
            words("go_now", None),
            "Going with 2 meanwhile; another pick undoes it."
        );
        let at = now + 3600;
        assert_eq!(
            words("go_later", Some(at)),
            format!("Goes with 2 at {} unless you pick.", clock(at, now))
        );
        assert_eq!(
            words("wait", Some(at)),
            format!("Waits for your pick; a reminder at {}.", clock(at, now))
        );
        assert_eq!(words("go_later", None), "Waits for your pick.");
        assert_eq!(words("wait", None), "Waits for your pick.");
        // The time is at the text's own weight; the words around it muted.
        let mut ask = call("c", &asked("go_later")).unwrap();
        ask.deadline = Some(at);
        let weights: Vec<bool> = meanwhile(&ask, now).unwrap().iter().map(|w| w.1).collect();
        assert_eq!(weights, [false, true, false]);
        // An ask from before agents said draws no such row.
        assert_eq!(meanwhile(&call("c", &keep_api()).unwrap(), now), None);
        // On the card, under the reason, barred.
        let card = card(
            &settled(call("c", &asked("go_now")).unwrap(), ""),
            &State::Open,
            false,
            60,
        );
        let shown: Vec<String> = card.rows.iter().map(|row| row.to_string()).collect();
        assert_eq!(
            shown[shown.len() - 2..],
            [
                "┃ Why 2: Nobody runs 0.3 outside this machine.",
                "┃ Going with 2 meanwhile; another pick undoes it.",
            ]
        );
        assert_eq!(card.hangs.len(), card.rows.len());
    }

    fn sign_in() -> Value {
        json!({
            "question": "Sign in to the package registry",
            "why": "Publishing the release needs your account; the token stays on your machine.",
            "command": "  npm login --auth-type=web  ",
            "if_unanswered": "go_now",
        })
    }

    #[test]
    fn a_step_only_the_person_can_take_is_an_ask_with_its_command_and_no_options() {
        let tool = tool();
        let required = tool["parameters"]["required"].as_array().unwrap();
        assert!(!required.contains(&json!("options")));
        assert!(tool["parameters"]["properties"]["command"].is_object());
        assert!(
            tool["description"]
                .as_str()
                .unwrap()
                .contains("put the exact line to run in")
        );
        let ask = call(&id(4, "c9"), &sign_in()).unwrap();
        assert_eq!(ask.command.as_deref(), Some("npm login --auth-type=web"));
        assert!(ask.options.is_empty());
        // Nothing to go ahead with: it waits, whatever was asked.
        assert_eq!(ask.unanswered, Some(Unanswered::Wait));
        // Still open until the person says how it went, as any ask.
        let items = [
            json!({"type": "function_call", "name": TOOL, "call_id": "c9", "turn": 4,
            "arguments": sign_in().to_string()}),
        ];
        assert_eq!(
            open(&items).map(|ask| ask.question).as_deref(),
            Some("Sign in to the package registry")
        );
        assert_eq!(
            answer(&ask, "done, signed in"),
            "You asked (ask:4/c9): Sign in to the package registry\n\ndone, signed in"
        );
        let hint: String = hint(&ask, 0, 1, 80)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            hint,
            "Answer “Sign in to the package registry”: say how it went · esc write instead"
        );
    }

    #[test]
    fn the_card_shows_the_line_to_run_where_a_click_copies_it() {
        let mut ask = call("c9", &sign_in()).unwrap();
        ask.deadline = None;
        let card = card(&ask, &State::Open, false, 60);
        let shown: Vec<String> = (card.rows.iter())
            .map(|row| row.to_string().trim_end().to_owned())
            .collect();
        assert_eq!(
            shown,
            [
                "? Sign in to the package registry",
                "┃ Publishing the release needs your account; the token stays",
                "┃ on your machine.",
                "┃",
                "┃ Run this in your own terminal; a click copies it.",
                "┃    npm login --auth-type=web",
                "┃",
                "┃ Waits for you to run it.",
            ]
        );
        assert_eq!(card.command, 5..6);
        assert!(card.options.is_empty());
        assert_eq!(card.hangs.len(), card.rows.len());
        // The line at the text's own weight; what it is, muted.
        assert!(!card.rows[5].spans.iter().any(|s| s.content.contains("npm")
            && s.style.add_modifier.contains(ratatui::style::Modifier::DIM)));
        let now = 1_800_000_000;
        ask.deadline = Some(now + 3600);
        let said: String = meanwhile(&ask, now)
            .unwrap()
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert_eq!(
            said,
            format!(
                "Waits for you to run it; a reminder at {}.",
                clock(now + 3600, now)
            )
        );
    }

    #[test]
    fn the_line_to_run_keeps_its_blank_lines_and_spaces() {
        let mut asked = sign_in();
        asked["command"] = json!("\n  cat <<'EOF' > notes.txt\nfirst  \n\nsecond\nEOF\n");
        let ask = call("c9", &asked).unwrap();
        assert_eq!(
            ask.command.as_deref(),
            Some("cat <<'EOF' > notes.txt\nfirst  \n\nsecond\nEOF")
        );
        let card = card(&ask, &State::Open, false, 60);
        let shown: Vec<String> = card.rows[card.command.clone()]
            .iter()
            .map(|row| row.to_string())
            .collect();
        assert_eq!(shown.len(), 5, "{shown:?}");
        assert_eq!(shown[2].trim_end(), "┃");
    }

    #[test]
    fn an_ask_has_options_or_a_command_never_both_nor_neither() {
        let mut both = sign_in();
        both["options"] =
            json!([{"label": "Yes", "explanation": "e"}, {"label": "No", "explanation": "f"}]);
        let refused = served(&both, "shop.fix", 1, "c1").unwrap_err();
        assert!(
            refused.starts_with("invalid_arguments: a command is a step"),
            "{refused}"
        );
        let mut neither = sign_in();
        neither.as_object_mut().unwrap().remove("command");
        let refused = served(&neither, "shop.fix", 1, "c1").unwrap_err();
        assert!(
            refused.starts_with("invalid_arguments: options to pick from"),
            "{refused}"
        );
    }

    #[test]
    fn a_clock_says_the_day_only_when_it_is_not_today() {
        let local = crate::trigger::local(1_800_000_000);
        // Noon that day, wherever the test runs.
        let noon =
            1_800_000_000 - (local.hour as i64 * 3600 + local.minute as i64 * 60) + 12 * 3600;
        assert_eq!(clock(noon + 90 * 60, noon), "13:30");
        assert_eq!(clock(noon + 24 * 3600, noon), "tomorrow 12:00");
        let later = crate::trigger::local(noon + 3 * 24 * 3600);
        assert_eq!(
            clock(noon + 3 * 24 * 3600, noon),
            format!("{:02}-{:02} 12:00", later.month, later.day)
        );
    }

    #[test]
    fn with_deadlines_off_every_ask_waits_even_one_going_now() {
        for plan in [Unanswered::Now, Unanswered::Later, Unanswered::Wait] {
            assert_eq!(allowed(plan, 0), Unanswered::Wait);
            assert_eq!(allowed(plan, 60), plan);
        }
    }

    #[test]
    fn a_deadline_is_a_whole_minute_never_early() {
        // 1_800_000_000 is a whole minute: an hour on is too.
        assert_eq!(deadline(60, 1_800_000_000), 1_800_000_000 + 3600);
        let at = deadline(1, 1_800_000_001);
        assert_eq!(at % 60, 0);
        assert!(at >= 1_800_000_061);
        assert_eq!(
            trigger_name("shop.lead", 12, "toolu_01AbCdEfGhIjK"),
            "shop.lead.ask-12-dEfGhIjK"
        );
        assert_eq!(trigger_name("a b", 3, "c-1"), "a-b.ask-3-c1");
    }

    #[test]
    fn a_fire_is_sonnys_words_and_says_what_to_do() {
        let later = fire("Keep the old API?\nmore", Unanswered::Later, "Drop it");
        assert!(later.contains("\"Keep the old API?\""));
        assert!(later.contains("go on now with \"Drop it\""));
        assert!(later.contains("undo it and follow their pick"));
        assert!(later.contains("This is Sonny, not the person"));
        let wait = fire("Tag it?", Unanswered::Wait, "");
        assert!(wait.contains("do not go ahead with it"));
        assert!(wait.contains("This is Sonny, not the person"));
    }

    #[test]
    fn an_ask_stays_open_until_answered_or_the_person_moves_on() {
        let asking = json!({"node": 1, "item": {"type": "function_call", "name": TOOL,
            "call_id": "a1", "arguments": asked("go_later").to_string()}});
        let person = |text: &str| {
            json!({"node": 9, "from": null, "origin": null,
            "item": {"role": "user", "content": [{"type": "input_text", "text": text}]}})
        };
        // Each entry of turn 2 unless it says another.
        let read = |entries: &[Value]| {
            let mut reading = Reading::default();
            (entries.iter()).find_map(|entry| {
                let at = entry["turn"].as_i64().unwrap_or(2);
                reading.entry(at, entry, 2, "a1")
            })
        };
        assert_eq!(read(std::slice::from_ref(&asking)), Some(true));
        // Words of their own after it: the conversation moved on.
        assert_eq!(
            read(&[person("Actually, hold on"), asking.clone()]),
            Some(false)
        );
        // Its own answer settles it; another question's does not.
        let ask = call(&id(2, "a1"), &asked("go_later")).unwrap();
        assert_eq!(
            read(&[person(&answer(&ask, "1")), asking.clone()]),
            Some(false)
        );
        assert_eq!(
            read(&[
                person("You asked (ask:2/a2): Tag it?\n\nYes"),
                asking.clone()
            ]),
            Some(true)
        );
        // Nor does the answer to an ask of another turn with the same
        // call: provider call IDs are unique only within a turn.
        let earlier = call(&id(1, "a1"), &asked("go_later")).unwrap();
        assert_eq!(
            read(&[person(&answer(&earlier, "1")), asking.clone()]),
            Some(true)
        );
        // A later turn's ask reusing the call, answered too, is another
        // ask: the reading goes on past it to this one's answer.
        let mut reused = asking.clone();
        reused["turn"] = json!(5);
        let later = call(&id(5, "a1"), &asked("go_later")).unwrap();
        assert_eq!(
            read(&[
                person(&answer(&later, "1")),
                reused,
                person(&answer(&ask, "1")),
                asking.clone()
            ]),
            Some(false)
        );
        // Nor does the answer to another ask of the same words: its
        // deadline still holds.
        let again = call(&id(2, "a3"), &asked("go_later")).unwrap();
        assert_eq!(again.question, ask.question);
        assert_eq!(
            read(&[person(&answer(&again, "1")), asking.clone()]),
            Some(true)
        );
        // An automation's, another agent's or a reaction are not the person.
        let automation = json!({"node": 8, "from": null, "origin": "automation",
            "item": {"role": "user", "content": [{"type": "input_text", "text": "look"}]}});
        let agent = json!({"node": 7, "from": "shop.lead", "origin": null,
            "item": {"role": "user", "content": [{"type": "input_text", "text": "carry on"}]}});
        let thumbs = person(&crate::reactions::message("👍", None));
        assert_eq!(
            read(&[automation, agent, thumbs, asking.clone()]),
            Some(true)
        );
        // Before the call is reached, nothing is known yet.
        assert_eq!(
            read(&[person("You asked (ask:2/a2): Tag it?\n\nYes")]),
            None
        );
    }

    #[test]
    fn the_reviewer_reads_each_ask_with_what_it_covers_and_the_persons_first_pick() {
        let asking = |call: &str, question: &str, covers: Value| {
            let arguments = json!({"question": question, "why": "w", "if_unanswered": "wait",
                "options": [{"label": "Yes", "explanation": "e"}, {"label": "No", "explanation": "e"}],
                "covers": covers});
            json!({"type": "function_call", "name": TOOL, "call_id": call,
                "arguments": arguments.to_string()})
        };
        let words = |text: String| json!({"role": "user", "content": [{"type": "input_text", "text": text}]});
        let publish_a = json!({"action": "publish", "target": "notes", "revision": "A"});
        let shown = call(
            &id(3, "a1"),
            &asking("a1", "Publish it?", publish_a.clone())["arguments"]
                .as_str()
                .and_then(|a| serde_json::from_str(a).ok())
                .unwrap(),
        )
        .unwrap();
        let mut from_lead = words(answer(&shown, "1"));
        from_lead["from"] = json!("shop.lead");
        // Newest first, as the approver reads history.
        let items = [
            // A later turn's refused call with the same ID refuses only its own.
            (
                7,
                json!({"type": "function_call_output", "call_id": "a1", "output": "", "failed": true}),
            ),
            (6, words(answer(&shown, "2"))),
            (5, words(answer(&shown, "1"))),
            (4, from_lead),
            // Refused before it reached the person: never asked.
            (
                4,
                json!({"type": "function_call_output", "call_id": "r1", "output": "", "failed": true}),
            ),
            (4, asking("r1", "Delete it?", json!(null))),
            // Asked in turn 3 again by call ID, and still open.
            (3, asking("a1", "Publish it?", publish_a)),
            (
                2,
                asking(
                    "a1",
                    "Send it?",
                    json!({"action": "send", "target": "the list"}),
                ),
            ),
        ];
        // Each of them bears on an ask but the lead's words; a reply or a
        // plain output does not either.
        let bearing: Vec<bool> = items.iter().map(|(_, item)| bears_on_asks(item)).collect();
        assert_eq!(bearing, [true, true, true, false, true, true, true, true]);
        let reply = json!({"role": "assistant", "content": [{"type": "output_text", "text": "x"}]});
        let output = json!({"type": "function_call_output", "call_id": "c", "output": "big"});
        assert!(
            !bears_on_asks(&reply)
                && !bears_on_asks(&output)
                && !bears_on_asks(&words("hi".into()))
        );
        let decisions = decisions(&items);
        let ids: Vec<&str> = decisions.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["ask:3/a1", "ask:2/a1"]);
        // The person's first pick counts; a lead's words and a later pick
        // don't.
        assert_eq!(decisions[0].answered, Some((5, "Yes".into())));
        assert_eq!(
            decisions[0].covers,
            Some(agent_client::approver::Covers {
                action: "publish".into(),
                target: "notes".into(),
                revision: Some("A".into()),
            })
        );
        // Another turn's ask with the same call ID is a decision of its own,
        // and open.
        assert_eq!(decisions[1].question, "Send it?");
        assert_eq!(decisions[1].answered, None);
        assert_eq!(decisions[1].covers.as_ref().unwrap().revision, None);
    }

    #[test]
    fn only_the_person_answers_an_ask_in_the_window_history_and_its_deadline() {
        let arguments = asked("go_later");
        let asking = json!({"node": 1, "item": {"type": "function_call", "name": TOOL,
            "call_id": "a1", "turn": 2, "arguments": arguments.to_string()}});
        let ask = call(&id(2, "a1"), &arguments).unwrap();
        let words = answer(&ask, "1");
        // The same words, from the person and from each other sender, as
        // history keeps them and as the transcript marks who sent them.
        let senders = [
            (json!({"from": null, "origin": null, "person": null}), true),
            (
                json!({"from": {"bot": "shop.lead", "turn": 3}, "origin": null, "person": null}),
                false,
            ),
            (
                json!({"from": null, "origin": "automation", "person": null}),
                false,
            ),
            (
                json!({"from": null, "origin": null, "person": "dana"}),
                false,
            ),
        ];
        for (sender, theirs) in senders {
            let from = crate::transcript::sender(
                sender["from"]["bot"].as_str(),
                sender["origin"].as_str(),
                sender["person"].as_str(),
            );
            let mut entry = sender.clone();
            entry["node"] = json!(9);
            entry["item"] =
                json!({"role": "user", "content": [{"type": "input_text", "text": words}]});
            // History and the window's list of open questions.
            let mut item = entry["item"].clone();
            if let Some(from) = &from {
                item["from"] = json!(from);
            }
            let history = open(&[item, asking["item"].clone()]);
            assert_eq!(history.is_none(), theirs, "{sender}");
            // The deadline.
            let mut reading = Reading::default();
            let deadline = [entry, asking.clone()]
                .iter()
                .find_map(|entry| reading.entry(2, entry, 2, "a1"));
            assert_eq!(deadline, Some(!theirs), "{sender}");
            // The card pinned live.
            let mut pinned = vec![ask.clone()];
            assert_eq!(
                close(&mut pinned, &words, from.as_deref()),
                theirs,
                "{sender}"
            );
            assert_eq!(answer_of(&words, from.as_deref()).is_some(), theirs);
        }
    }
}
