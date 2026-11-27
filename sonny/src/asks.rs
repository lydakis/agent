//! Decisions for the person: the lead calls the `ask` tool Sonny declares
//! on it, which the daemon records, and Sonny draws the question in the
//! conversation where it was asked: why it is asked, each option with what
//! picking it means, the one the lead recommends and why, and what the
//! agent does while nobody answers. A pick (a click on an option, or its
//! number typed) answers it, and so do words typed while it is in focus in
//! the composer, the row over them naming it: either goes to the lead as
//! their next message, once. Words written after Esc are the person's own,
//! never framed as an answer, and change no card: the agent reads whether
//! they answer it, as a reply under a card in a chat is read, and closes
//! it with [`CLOSE`] once it knows. A card stays open until a pick or that
//! close, as a decision card in a Claude project thread does, and says
//! which of these it is in ([`State`]). The open questions are read from
//! the history Sonny replays: its `ask` calls not yet answered nor closed.
//!
//! Silence counts as a yes only where the agent says a step can be undone,
//! as an Apache project's lazy consensus passes what is easy to revert once
//! its stated time is up. The agent marks each ask `one_way` or not: one
//! that can be undone goes on with what it recommends once the person's
//! wait passes (at once when they set none), a one-way door waits for
//! their pick ([`Unanswered`]). The card says which, and the tool host
//! that answers the call ([`served`]) sets a one-off trigger at the
//! deadline.
//! Its `--if` is `sonny ask open` ([`still_open`]): once the person
//! picked, or the agent closed it, nothing fires; words of the person's
//! own cancel nothing, so a card the agent did not close goes ahead at
//! its deadline. An answer that opened a turn, or the agent's close,
//! removes it at once ([`let_deadline_go`]). A fire is Sonny's words in a trigger's turn,
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
/// The longest wait a question may have: a week.
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
    /// What to pick from; none for a step only the person can take, which
    /// `why` says how to take.
    pub options: Vec<Choice>,
    /// The option recommended, from 0; `why` says why.
    pub recommended: Option<usize>,
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
    /// Whether it was asked under the rule that a card stays open until a
    /// pick or the agent's close, as its call's result says
    /// ([`STAYS_OPEN`]). One asked before that rule was settled, as every
    /// window showed it then, by the person's own words after it
    /// ([`State::Aside`]), and is read so still. One whose result is not
    /// read yet is asked under the rule of now.
    #[serde(default = "stays_open")]
    pub stays_open: bool,
}

/// What an ask's result says it was asked under, from the release on
/// that keeps a card open until a pick or a close: an ask's result
/// without it was asked before, under the rule that the person's own
/// words after it set it aside.
pub const STAYS_OPEN: &str = "stays_open";

fn stays_open() -> bool {
    true
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

/// Where a question stands, as its card says it: one of open, answered
/// or withdrawn, answered either by the person or, from their words, by
/// the agent's own record. Open until one of the others.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// It waits on the person.
    Open,
    /// The person answered it: the option they picked, or their words.
    Answered(String),
    /// The agent recorded that the person's words answered it, going with
    /// an option (from 0) or none, and its note of what they said. Its
    /// record, never their answer: a pick still answers it.
    Recorded { option: Option<usize>, note: String },
    /// The agent took it back as no longer needed, and why.
    Withdrawn(String),
    /// Asked before cards stayed open until a pick or a close
    /// ([`Ask::stays_open`]), the person wrote words of their own past it:
    /// set aside then, as every window showed it, and so it stays. A pick
    /// still answers it.
    Aside,
}

impl State {
    /// Whether it is answered, closed or set aside: its card folds, and
    /// it waits on nobody.
    pub fn settled(&self) -> bool {
        *self != Self::Open
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

/// What an agent does while the person has not answered, as Sonny took
/// its ask: what `one_way` said, with the person's wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Unanswered {
    /// Goes on with what it recommends at once: a step it can undo, with
    /// no wait set.
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
            choice, or a step that cannot be undone, where their pick changes what you build. Check \
            `sonny ask list` first: an answer there may settle yours. Ask once, one decision per \
            call, and leave it out of your reply. A step only the person can take, such as a \
            sign-in or a command in their own terminal, is an ask with no options, the exact line \
            to run in why. Any other question with nothing to choose between goes in your reply. \
            Their answer comes as their next message, starting \"You asked\"; once their words \
            answer it, or it no longer matters, close it with close_ask.",
        "parameters": {
            "type": "object",
            "properties": {
                "question": {"type": "string",
                    "description": "The question, verb first, in the person's own words, in about twelve words."},
                "why": {"type": "string",
                    "description": "What it decides, the facts the person needs, how each option works and why you recommend yours, in a sentence or three: this card may be all they see. For a step of theirs, what it unblocks and the exact line to run, whole, in a code block."},
                "options": {"type": "array",
                    "description": "2 to 4 options, each a different way forward; none for a step of theirs.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": {"type": "string", "description": "The option, in a few words."},
                            "explanation": {"type": "string",
                                "description": "What the person will see, keep, wait for or give up if they pick it, one sentence in their words; why says how it works."}
                        },
                        "required": ["label", "explanation"],
                        "additionalProperties": false
                    }},
                "recommended": {"type": "integer",
                    "description": "The number of the option you recommend, from 1."},
                "one_way": {"type": "boolean",
                    "description": "true when it goes out in their name or to other people, \
                        approves, merges, publishes, deletes, spends or cannot be undone, or you \
                        are unsure: you wait for their pick. false: unless they answer first, \
                        Sonny has you go on with the option you recommend once their wait passes; \
                        if they pick another later, undo it. A step of theirs always waits."},
                "covers": {"type": "object",
                    "description": "What a yes approves: that action, target and revision alone; \
                        another needs a new ask.",
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
                    "additionalProperties": false}
            },
            "required": ["question", "why", "one_way"],
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
/// by one blank line; a fenced code block, such as a step's line to run,
/// kept as written, since a blank line or a space in a quoted value or a
/// here-document is part of what it does.
fn prose(text: Option<&str>) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut fenced = false;
    for line in text.unwrap_or("").lines() {
        let fence = ["```", "~~~"]
            .iter()
            .any(|f| line.trim_start().starts_with(f));
        if fenced && !fence {
            out.push(line);
            continue;
        }
        fenced ^= fence;
        let line = line.trim_end();
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
    let recommended = arguments["recommended"]
        .as_u64()
        .and_then(|n| (n as usize).checked_sub(1))
        .filter(|at| *at < options.len());
    // Going ahead goes ahead with a recommendation; with none it waits,
    // as a step of the person's does. Its result says when ([`settled`]).
    let unanswered = arguments["one_way"].as_bool().map(|one_way| {
        match one_way || options.is_empty() || recommended.is_none() {
            true => Unanswered::Wait,
            false => Unanswered::Later,
        }
    });
    (!question.is_empty()).then(|| Ask {
        id: id.to_owned(),
        question,
        why: prose(arguments["why"].as_str()),
        options,
        recommended,
        unanswered,
        deadline: None,
        covers: Covers::of(&arguments["covers"]),
        task: None,
        stays_open: true,
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
    ask.stays_open = said[STAYS_OPEN] == true;
    ask
}

/// Whether a call's kept result `output` says its ask was asked under the
/// rule that a card stays open until a pick or a close ([`STAYS_OPEN`]):
/// only an ask's result is read so, once its call is known to be one.
pub fn says_stays_open(output: &str) -> bool {
    output.contains(STAYS_OPEN)
        && serde_json::from_str::<Value>(output).is_ok_and(|said| said[STAYS_OPEN] == true)
}

/// Whether a message is the person's own words, answering no question:
/// not another agent's or a client's, nor a reaction, a note taken away
/// or what they ran. Before cards stayed open until a pick or a close,
/// such words set aside every question asked before them; an ask asked
/// then is read so still ([`Ask::stays_open`]).
pub fn moves_on(text: &str, from: Option<&str>) -> bool {
    answered(text).is_none()
        && !crate::transcript::relays(text, from)
        && !crate::reactions::is_reaction(text)
        && crate::margin::closed(text).is_none()
        && crate::user_shell::read(text).is_none()
}

/// The oldest question still open among `bot`'s `items`, newest first,
/// whole, as its card shows it, under the ID [`id`] gives it from the
/// `turn` each call is marked with: an `ask` call that was
/// taken, with no answer to it after, as the pinned questions are closed:
/// by the person's `You asked` message naming that ask, one answer to one
/// ask, or by the agent's close. Words of the person's own close none,
/// but set aside one asked before cards stayed open ([`Ask::stays_open`]).
/// Asks are told apart by turn and call, never by wording: two asking the
/// same question are two decisions.
pub fn open(items: &[Value]) -> Option<Ask> {
    let (mut answers, mut refused, mut open) = (Vec::new(), Vec::new(), None);
    // Each call's result, by turn and call, as provider IDs repeat across
    // turns: what Sonny took an ask as, its plan and deadline, is there.
    let mut results: Vec<((Value, Value), String)> = Vec::new();
    // The person wrote words of their own after what is read now.
    let mut moved_on = false;
    for item in items {
        let at = (item["turn"].clone(), item["call_id"].clone());
        if item["role"] == "user" {
            let (text, from) = (crate::text(item), item["from"].as_str());
            match answer_of(&text, from) {
                Some(call) => answers.push(call.to_owned()),
                None => moved_on |= moves_on(&text, from),
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
                None => {
                    // One whose turn is not known no answer names.
                    ask.id = (asked.or_else(|| item["call_id"].as_str().map(str::to_owned)))
                        .unwrap_or_default();
                    if let Some((_, said)) = results.iter().find(|(of, _)| *of == at) {
                        ask = settled(ask, said);
                    }
                    // Asked before cards stayed open, the person's words
                    // after it set it aside then.
                    if ask.stays_open || !moved_on {
                        open = Some(ask);
                    }
                }
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

/// Whether running a `!` command in this window answers `ask`: a step
/// only the person can take (an ask with nothing to pick), the window's
/// agent's own, whose command the person runs here as Claude Code has
/// them run theirs. A task's goes to the task, which would not see what
/// ran in its lead's window.
pub fn runs_here(ask: &Ask) -> bool {
    ask.options.is_empty() && ask.task.is_none()
}

/// The person's answer to a step of theirs they ran here with `!`, as
/// [`answer`] frames it: what they ran and how it exited. What it printed
/// goes on its own just before, as their command's record, never as their
/// words: it is no consent to anything.
pub fn ran(ran: &crate::user_shell::Ran) -> String {
    let mut lines = ran.command.lines();
    let first = lines.next().unwrap_or_default().trim();
    let more = if lines.next().is_some() { " …" } else { "" };
    format!("Ran ! {first}{more} · exit {}", ran.exit)
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
    /// What each row's rows past its first start with, should it be
    /// drawn narrower than it was laid: the bar, then room to the text,
    /// so the bar goes on down however the rows wrap.
    pub hangs: Vec<Line<'static>>,
    /// The rows the question is on, a click on which brings it to the
    /// composer, or opens or folds a settled card.
    pub question: std::ops::Range<usize>,
}

/// The decision as it is drawn where it was asked: the question, why,
/// and each option by number with what it means, the one recommended
/// marked so.
/// A `?` in yellow, what needs the person, starts it, and a muted bar
/// down its left side sets it apart from the replies around it, as a
/// callout's does: a shape of its own without color too. Where it
/// stands, when not open, is its last row: answered it starts with a
/// `✓`, withdrawn with a `–`, and `folded`
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
        State::Answered(_) | State::Recorded { .. } => {
            (Span::raw(TICK).fg(Color::Green), Style::new())
        }
        State::Withdrawn(_) => (Span::raw(DASH).dim(), Style::new()),
        State::Aside => (Span::raw(GLYPH).dim(), Style::new()),
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
    if !ask.options.is_empty() {
        rows.push(spacer());
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
    // What happens while nobody answers, under the options, muted, the
    // time at the text's own weight: only while it is open, past which
    // nothing fires.
    if *state != State::Open {
        rows.push(spacer());
        stands(&mut rows);
    } else if let Some(meanwhile) = meanwhile(ask, now()) {
        rows.push(spacer());
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
        State::Answered(answer) => vec![muted("Answered · "), plain(answer)],
        State::Aside => vec![muted(
            "Set aside: you wrote past it. A pick still answers it.",
        )],
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
    let plan = ask.unanswered?;
    Some(until_picked(
        plan,
        ask.recommended,
        ask.deadline,
        ask.options.is_empty(),
        now,
    ))
}

/// What the agent does until the person answers, in the card's words:
/// `plan`, the option it goes with (`pick`, from 0), when (`deadline`),
/// and whether it is a step of the person's (`step`), `now` in seconds
/// since the epoch. A question's reminder in `/background` says the same.
pub fn until_picked(
    plan: Unanswered,
    pick: Option<usize>,
    deadline: Option<i64>,
    step: bool,
    now: i64,
) -> Vec<(String, bool)> {
    let pick = pick.map(|at| (at + 1).to_string());
    let muted = |text: &str| (text.to_owned(), false);
    let at = |deadline: i64| (clock(deadline, now), true);
    match (plan, pick, deadline) {
        (Unanswered::Now, Some(pick), _) => vec![muted(&format!(
            "Going with {pick} meanwhile; another pick undoes it."
        ))],
        (Unanswered::Later, Some(pick), Some(deadline)) => vec![
            muted(&format!("Goes with {pick} at ")),
            at(deadline),
            muted(" unless you pick."),
        ],
        (Unanswered::Wait, _, Some(deadline)) if step => vec![
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
        _ if step => vec![muted("Waits for you to run it.")],
        _ => vec![muted("Waits for your pick.")],
    }
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
/// answers, and the one-off trigger that wakes it at the deadline, the
/// person's wait. One that can be undone goes on with what it recommends
/// then, or at once with no wait set; a one-way door, or a step of the
/// person's, waits for them and is reminded then, or never with no wait
/// set. The result says when, for the card and the agent.
pub fn served(
    arguments: &Value,
    bot: &str,
    turn: i64,
    call: &str,
    shell: &crate::trigger::Shell,
) -> Result<String, String> {
    let ask = self::call(&id(turn, call), arguments)
        .ok_or("invalid_arguments: a question to ask, in question")?;
    let one_way =
        (arguments["one_way"].as_bool()).ok_or("invalid_arguments: one_way is true or false")?;
    // A step only the person can take waits for them, whatever was asked.
    let step = ask.options.is_empty();
    if !step && !one_way && ask.recommended.is_none() {
        return Err(
            "invalid_arguments: going ahead needs the option you recommend, in recommended".into(),
        );
    }
    let label = ask
        .recommended
        .map(|at| ask.options[at].label.clone())
        .unwrap_or_default();
    let minutes = agent_client::config::load()
        .map(|config| config.sonny.ask_wait_minutes)
        .unwrap_or(60)
        .min(MAX_MINUTES);
    let plan = match (step || one_way, minutes) {
        (true, _) => Unanswered::Wait,
        (false, 0) => Unanswered::Now,
        (false, _) => Unanswered::Later,
    };
    let deadline = (plan != Unanswered::Now && minutes > 0).then(|| deadline(minutes, now()));
    // A deadline Sonny could not set leaves the ask waiting, and says why:
    // the card still shows, and nothing goes ahead unseen.
    let (plan, deadline, unset) =
        match deadline.map(|at| set_deadline(&ask, plan, &label, at, (bot, turn, call), shell)) {
            Some(Err(error)) => (Unanswered::Wait, None, Some(error)),
            _ => (plan, deadline, None),
        };
    let when = deadline.map(|d| clock(d, now()));
    let next = match (plan, &when, &unset) {
        _ if step => format!(
            "Shown to the person as a step of theirs. Do nothing that needs it until they say how \
             it went, in their next message.{} Meanwhile do work that does not depend on it, or \
             end your turn.",
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
             {when} to go on with \"{label}\"; if they pick another after, undo it and follow \
             their pick. Until then do work that does not depend on the answer, or end your turn."
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
        _ => "Shown to the person. Do nothing they have not picked; their setting has no \
             reminder. Meanwhile do work that does not depend on the answer, or end your turn."
            .to_owned(),
    };
    Ok(
        json!({"id": ask.id, "if_unanswered": plan.name(), "deadline_s": deadline,
        "deadline": when, "deadline_error": unset, STAYS_OPEN: true, "next": next})
        .to_string(),
    )
}

/// The one-off trigger that wakes `bot` at `deadline` about its ask, unless
/// `sonny ask open` finds it settled by then.
fn set_deadline(
    ask: &Ask,
    plan: Unanswered,
    label: &str,
    deadline: i64,
    (bot, turn, call): (&str, i64, &str),
    shell: &crate::trigger::Shell,
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
        &fire(
            &ask.question,
            plan,
            ask.recommended,
            label,
            ask.options.is_empty(),
        ),
        shell,
    )
    .map(|_| ())
}

/// The whole minute `minutes` from `now`, never before it, as a one-off
/// trigger keeps time.
fn deadline(minutes: u64, now: i64) -> i64 {
    let at = now + minutes as i64 * 60;
    at + (60 - at.rem_euclid(60)) % 60
}

/// Lets the deadline of `bot`'s ask `asked` go once nothing is left for
/// it to remind of: the person answered it, or it was closed. Its one-off
/// trigger leaves `/background` then, not at its time; none there (no
/// deadline, or gone already) is fine. Should this miss, its gate still
/// says no at the deadline.
pub fn let_deadline_go(bot: &str, asked: &str) {
    if let Some((turn, call)) = named(asked) {
        let name = trigger_name(bot, turn, call);
        let _ = crate::trigger::tool(
            &json!({"remove": name}),
            &crate::trigger::Caller::default(),
            &crate::trigger::Shell::default(),
        );
    }
}

/// The turn and call ask `asked` names, as [`id`] wrote it.
fn named(asked: &str) -> Option<(i64, &str)> {
    let (turn, call) = asked.strip_prefix("ask:")?.split_once('/')?;
    (!call.is_empty()).then_some((turn.parse().ok()?, call))
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
/// person's, so it allows nothing that needs theirs. It says the option
/// it goes with (`pick`, from 0) by its number and label, as the card
/// does, and a `step` of the person's as one; [`reminder`] reads it back.
pub(crate) fn fire(
    question: &str,
    plan: Unanswered,
    pick: Option<usize>,
    label: &str,
    step: bool,
) -> String {
    let first = question.lines().next().unwrap_or_default();
    let question = crate::transcript::cut(first, QUOTED);
    let label = crate::transcript::cut(label.lines().next().unwrap_or_default(), QUOTED);
    let tail = "This is Sonny, not the person: it is no answer of theirs, and it allows nothing \
        that needs their own words.";
    match (plan, pick) {
        (Unanswered::Later, Some(pick)) => format!(
            "{LATER}{question}{LATER_GOES}{} \"{label}\", the option you recommended, as you said \
             you would. Keep it undoable, and if they pick another later, undo it and follow their \
             pick. {tail}",
            pick + 1
        ),
        _ if step => format!(
            "{STEP}{question}{STEP_WAITS} so do not go ahead with what needs it. Go on with what \
             does not depend on it. {tail}"
        ),
        _ => format!(
            "{WAIT}{question}{WAIT_WAITS} person, so do not go ahead with it. Go on with what does \
             not depend on it. {tail}"
        ),
    }
}

/// How a fire's message opens and goes on past its question, for each
/// plan: [`fire`] writes them and [`reminder`] reads them.
const LATER: &str = "Nobody answered your question \"";
const LATER_GOES: &str = "\" in time, so go on now with option ";
const STEP: &str = "Nobody has said yet how your step went: \"";
const STEP_WAITS: &str = "\" waits for the person to run it,";
const WAIT: &str = "Your question \"";
const WAIT_WAITS: &str = "\" still has no answer, and you said it waits for the";

/// A question's reminder, as `/background` lists it: Sonny's deadline for
/// an agent's open question, the one-off automation [`set_deadline`]
/// adds, read from its `--if`, its time and its message.
#[derive(Clone, Debug, PartialEq)]
pub struct Reminder {
    /// The agent that asked.
    pub asker: String,
    /// The question's first line, as the fire quotes it.
    pub question: String,
    pub plan: Unanswered,
    /// The option it goes with, from 0.
    pub pick: Option<usize>,
    /// A step only the person can take, not a pick.
    pub step: bool,
    /// When, in seconds since the epoch.
    pub at: i64,
}

impl Reminder {
    /// What it does unless the person answers first, in the card's words,
    /// `now` in seconds since the epoch.
    pub fn says(&self, now: i64) -> Vec<(String, bool)> {
        until_picked(self.plan, self.pick, Some(self.at), self.step, now)
    }
}

/// The reminder an automation is, when it is a question's: its `--if`
/// (`gate`) is `'SONNY' ask open 'BOT' TURN 'CALL'`, it runs once `when`
/// (`at YYYY-MM-DD HH:MM`), and its message is a [`fire`].
pub fn reminder(gate: &str, when: &str, message: &str) -> Option<Reminder> {
    let asker = {
        let (_, rest) = gate.split_once("' ask open '")?;
        let mut asker = String::new();
        let mut chars = rest.chars().peekable();
        // A quote in the name is written `'\''`.
        loop {
            match chars.next()? {
                '\'' if chars.peek() == Some(&'\\') => {
                    // The backslash, its quote and the quote that opens
                    // the rest.
                    for _ in 0..3 {
                        chars.next();
                    }
                    asker.push('\'');
                }
                '\'' => break,
                c => asker.push(c),
            }
        }
        asker
    };
    let at = {
        let at = when.strip_prefix("at ")?;
        let (date, time) = at.split_once(' ')?;
        let mut date = date.split('-');
        let (hour, minute) = time.split_once(':')?;
        crate::trigger::epoch_of(crate::trigger::Local {
            year: date.next()?.parse().ok()?,
            month: date.next()?.parse().ok()?,
            day: date.next()?.parse().ok()?,
            hour: hour.parse().ok()?,
            minute: minute.parse().ok()?,
        })?
    };
    let between = |head: &str, tail: &str| -> Option<(String, String)> {
        let rest = message.strip_prefix(head)?;
        let (question, after) = rest.split_once(tail)?;
        Some((question.to_owned(), after.to_owned()))
    };
    let (question, plan, pick, step) = if let Some((question, after)) = between(LATER, LATER_GOES) {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        let pick = digits.parse::<usize>().ok()?.checked_sub(1)?;
        (question, Unanswered::Later, Some(pick), false)
    } else if let Some((question, _)) = between(STEP, STEP_WAITS) {
        (question, Unanswered::Wait, None, true)
    } else {
        let (question, _) = between(WAIT, WAIT_WAITS)?;
        (question, Unanswered::Wait, None, false)
    };
    Some(Reminder {
        asker,
        question,
        plan,
        pick,
        step,
        at,
    })
}

/// Where an ask stands, read from its agent's history newest first, one
/// entry at a time, back to the call that asked it, as [`open`] reads it:
/// answered by the person, closed by the agent, set aside by the person's
/// own words after it when asked before cards stayed open, or open.
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
    /// The calls that failed or were refused whose call is not read yet,
    /// by turn and call, let go the same way.
    refused: Vec<(i64, Value)>,
    /// Of `taken`, those whose result says its ask stays open until a
    /// pick or a close ([`says_stays_open`]), let go the same way.
    marked: Vec<(i64, Value)>,
    /// The person wrote words of their own after what is read now.
    moved_on: bool,
    /// The one ask read for, when only its answers and closes are kept,
    /// so a long history costs no more than that ask's.
    wanted: Option<String>,
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
        let (made, came, stays_open) = (items.iter())
            .find(|(item, ..)| item["type"] == "function_call" && item["call_id"] == call)?;
        let asked = id(turn, call);
        // A call that failed or was refused put no question, as no card
        // shows one: its deadline, set as it was answered, has nothing
        // to remind of, while the agent may well ask it again.
        let ask = (made["arguments"].as_str())
            .filter(|_| *came != Came::Refused)
            .and_then(|a| serde_json::from_str::<Value>(a).ok())
            .and_then(|arguments| self::call(&asked, &arguments));
        Some(ask.map(|mut ask| {
            ask.stays_open = *stays_open;
            let state = self.state(&asked, *stays_open);
            (ask, state)
        }))
    }

    /// Takes in what `entry`, of turn `at`, says of the asks before it:
    /// answers, closes, results and the person's own words. Its calls,
    /// each with what came of it and whether its result says its ask
    /// stays open until a pick or a close (one not taken yet does).
    fn read(&mut self, at: i64, entry: &Value) -> Vec<(Value, Came, bool)> {
        let items = crate::responses(&entry["item"]);
        let mut read = Vec::with_capacity(items.len());
        let wanted = |of: &str| self.wanted.as_deref().is_none_or(|wanted| wanted == of);
        // An entry's results first, as they come after its calls.
        for item in items
            .iter()
            .filter(|item| item["type"] == "function_call_output")
        {
            let made = (at, item["call_id"].clone());
            match item["failed"] == true || crate::reactions::refused(item) {
                true => self.refused.push(made),
                false => {
                    if item["output"].as_str().is_some_and(says_stays_open) {
                        self.marked.push(made.clone());
                    }
                    self.taken.push(made)
                }
            }
        }
        let mut closes = Vec::new();
        for item in items.iter().filter(|item| item["type"] == "function_call") {
            let made = (at, item["call_id"].clone());
            let mut came = Came::Running;
            for (list, so) in [
                (&mut self.taken, Came::Taken),
                (&mut self.refused, Came::Refused),
            ] {
                if let Some(at) = list.iter().position(|result| *result == made) {
                    list.swap_remove(at);
                    came = so;
                }
            }
            let stays_open = match self.marked.iter().position(|result| *result == made) {
                Some(at) => {
                    self.marked.swap_remove(at);
                    true
                }
                None => came != Came::Taken,
            };
            read.push((item.clone(), came, stays_open));
            if let Some((closed, state)) = closed_by(item)
                && self.skip.as_ref() != Some(&made)
                && came == Came::Taken
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
                    if wanted(answered) {
                        self.answers
                            .push((answered.to_owned(), answer.trim().to_owned()));
                    }
                }
                _ => self.moved_on |= moves_on(&text, None),
            }
        }
        read
    }

    /// Where ask `asked` stands by what was read after it. The person's
    /// first answer counts, as the daemon takes only the first, over any
    /// close; then the agent's first close that was taken; then, for one
    /// asked before cards stayed open (not `stays_open`), the person's own
    /// words after it, which set it aside.
    fn state(&self, asked: &str, stays_open: bool) -> State {
        if let Some((_, answer)) = (self.answers.iter().rev()).find(|(of, _)| of == asked) {
            return State::Answered(answer.clone());
        }
        let closed = (self.closes.iter().rev()).find(|(of, _)| of == asked);
        match closed {
            Some((_, state)) => state.clone(),
            None if !stays_open && self.moved_on => State::Aside,
            None => State::Open,
        }
    }
}

/// What came of a call, as [`Standing`] reads its result.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Came {
    /// Its result says it was taken.
    Taken,
    /// It failed, or the daemon refused it.
    Refused,
    /// No result yet: still running, or cut before it had one.
    Running,
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
            .filter(|(item, came, _)| *came == Came::Taken && item["name"] == TOOL)
            .filter_map(|(item, _, stays_open)| {
                let asked = id(at, item["call_id"].as_str()?);
                let arguments = serde_json::from_str::<Value>(item["arguments"].as_str()?).ok()?;
                let mut ask = self::call(&asked, &arguments)?;
                ask.stays_open = *stays_open;
                Some((ask, self.0.state(&asked, *stays_open)))
            })
            .collect()
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
    let (turn, call) = (named(&asked))
        .map(|(turn, call)| (turn, call.to_owned()))
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
    // Settled before this close, it has nothing left to remind of either:
    // its deadline goes as it would with the close.
    let settled = match now {
        State::Answered(answer) => Some(format!(
            "answered: the person answered it themselves (\"{}\"), and their answer stands",
            crate::transcript::cut(answer.lines().next().unwrap_or(""), QUOTED)
        )),
        State::Recorded { .. } | State::Withdrawn(_) => {
            Some(format!("closed: {asked} is closed already"))
        }
        State::Open | State::Aside => None,
    };
    if let Some(refusal) = settled {
        let (bot, gone) = (bot.to_owned(), asked.clone());
        let _ = tokio::task::spawn_blocking(move || let_deadline_go(&bot, &gone)).await;
        return Err(refusal);
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
    let (bot, gone) = (bot.to_owned(), asked.clone());
    let _ = tokio::task::spawn_blocking(move || let_deadline_go(&bot, &gone)).await;
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
    let found = standing(&client, bot, turn, call, None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(found.is_some_and(|(_, state)| state == State::Open))
}

/// Whether `bot` waits on the person, read from its history newest first,
/// all of it if need be, as a question stays open until a pick or the
/// agent's close: one of its questions still open that does not go ahead
/// by itself, or goes ahead only at a deadline not yet past (`now`, in
/// seconds since the epoch).
pub async fn waits(
    client: &agent_client::Client,
    bot: &str,
    now: i64,
) -> agent_client::Result<bool> {
    let (mut asked, mut said, mut waits) =
        (Asked::default(), std::collections::HashMap::new(), false);
    crate::said::walk(client, bot, 0, |at, entry| {
        // A call's result, what Sonny took the ask as, comes after it, so
        // is read first.
        for item in crate::responses(&entry["item"]) {
            if item["type"] == "function_call_output"
                && let Some(call) = item["call_id"].as_str()
                && let output = crate::output(&item).0
                && output.contains("if_unanswered")
            {
                said.insert(id(at, call), output);
            }
        }
        waits = (asked.entry(at, entry).into_iter()).any(|(ask, state)| {
            let ask = match said.get(&ask.id) {
                Some(output) => settled(ask, output),
                None => ask,
            };
            state == State::Open
                && match ask.unanswered {
                    Some(Unanswered::Now) => false,
                    Some(Unanswered::Later) => ask.deadline.is_none_or(|at| at > now),
                    Some(Unanswered::Wait) | None => true,
                }
        });
        waits
    })
    .await?;
    Ok(waits)
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
/// `of` open, in `width` columns: which question, muted, then its keys in
/// the one grammar.
pub fn hint(ask: &Ask, at: usize, of: usize, width: usize) -> Vec<Span<'static>> {
    let fitted = fit_hint(ask, at, of, width);
    let said = crate::transcript::cut(&fitted.said(), width);
    let mut spans = vec![Span::raw(said).dim()];
    if !fitted.keys.is_empty() {
        spans.push(Span::raw(" ").dim());
        spans.extend(crate::hint::spans(&fitted.keys));
    }
    spans
}

/// The task's name as the composer's hint for `ask` shows it in `width`
/// columns, shortened or whole, for its link to open the task.
pub fn hint_name(ask: &Ask, at: usize, of: usize, width: usize) -> Option<String> {
    fit_hint(ask, at, of, width).name
}

/// The answer hint's parts, each as much of it as the row has room for.
struct Hint {
    question: String,
    /// The task's name, `from` it, when the question is a task's.
    name: Option<String>,
    /// Which of several it is: ` (1 of 2)`.
    place: String,
    /// The words before the keys: ` type or`.
    words: &'static str,
    keys: String,
}

impl Hint {
    /// The muted words before the keys.
    fn said(&self) -> String {
        let whose = (self.name.as_ref()).map_or(String::new(), |name| format!(" from {name}"));
        format!(
            "Answer “{}”{whose}{}:{}",
            self.question, self.place, self.words
        )
    }

    fn width(&self) -> usize {
        let keys = match self.keys.is_empty() {
            true => 0,
            false => 1 + self.keys.width(),
        };
        self.said().width() + keys
    }
}

/// The answer hint in `width` columns, by display width. Short of room it
/// gives way in this order, a later part only once every earlier one has
/// given all it gives: the question shortens with `…` down to 8 columns;
/// its keys go as any line of keys does, `esc write instead` staying, and
/// `type or` with the pick; a step's `say how it went`; which of several
/// it is; then a task's name shortens with `…` while three of its letters
/// show, else goes whole. Narrower still, the question shortens past its
/// 8 columns. Nothing is cut without a `…`, and a key is whole or gone.
fn fit_hint(ask: &Ask, at: usize, of: usize, width: usize) -> Hint {
    const QUESTION: usize = 8;
    const NAME_LETTERS: usize = 3;
    let pick = match ask.options.len() {
        0 => None,
        1 => Some("1".to_owned()),
        n => Some(format!("1–{n}")),
    };
    let mut keys = crate::hint::Keys::new();
    if let Some(pick) = &pick {
        keys = keys.key(pick.clone(), "pick");
    }
    if of > 1 {
        keys = keys.key("↑↓", "switch");
    }
    let keys = keys.esc(crate::hint::Esc::WriteInstead);
    // Words answer it too: typed, or for a step of the person's, how it
    // went, or the step itself run here.
    let words = match ask.options.len() {
        0 if runs_here(ask) => " say how it went or ! run it ·",
        0 => " say how it went ·",
        _ => " type or",
    };
    let question = plain(&ask.question);
    let mut hint = Hint {
        question: String::new(),
        // A task's question, pinned in its lead's window, by whose it is.
        name: ask.task.as_ref().map(|task| task.name.clone()),
        place: place(at, of),
        words,
        keys: keys.text(),
    };
    // What the rest leaves the part being fitted.
    let room = |hint: &Hint, part: usize| width.saturating_sub(hint.width() - part);
    // 1. The question, down to 8 columns.
    let room_for_question = room(&hint, 0);
    hint.question = crate::transcript::cut(&question, room_for_question.max(QUESTION));
    if hint.width() <= width {
        return hint;
    }
    // 2. Its keys, as any line of keys gives way. `type or` goes with the
    // pick: the two are one phrase.
    hint.keys = keys.fit(room(&hint, hint.keys.width()));
    if pick.is_some_and(|pick| !hint.keys.contains(&format!("{pick} pick"))) {
        hint.words = "";
    }
    if hint.width() <= width {
        return hint;
    }
    // 3. A step's words, how it went.
    hint.words = "";
    if hint.width() <= width {
        return hint;
    }
    // 4. Which of several it is.
    hint.place = String::new();
    if hint.width() <= width {
        return hint;
    }
    // 5. A task's name, shortened while three of its letters show, else
    // gone whole with its `from`: its card says whose it is.
    if let Some(name) = hint.name.take() {
        let room = room(&hint, 0).saturating_sub(" from ".width());
        let shown = crate::transcript::cut(&name, room);
        let letters = shown.trim_end_matches('…').chars().count();
        if shown == name || letters >= NAME_LETTERS {
            hint.name = Some(shown);
        }
        if hint.width() <= width {
            return hint;
        }
    }
    // Narrower still, the question shortens past its 8 columns; then the
    // keys go, whole, and what is left is cut with `…` where it is drawn.
    hint.question = crate::transcript::cut(&question, room(&hint, hint.question.width()));
    if hint.width() <= width {
        return hint;
    }
    hint.keys = String::new();
    hint
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

    /// Whether ask `call` of turn `turn` is open, read entry by entry as
    /// [`still_open`] reads it: `None` until the call is reached.
    #[derive(Default)]
    struct Reading(Standing);

    impl Reading {
        fn entry(&mut self, at: i64, entry: &Value, turn: i64, call: &str) -> Option<bool> {
            (self.0.entry(at, entry, turn, call))
                .map(|found| found.is_some_and(|(_, state)| state == State::Open))
        }
    }

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
            "recommended": 2
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
            (covers["description"].as_str().unwrap())
                .contains("that action, target and revision alone; another needs a new ask")
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
    fn a_step_to_run_here_says_a_bang_answers_it_and_how_it_went() {
        let mut step = call(
            "ask:1/c1",
            &json!({"question": "Sign in?", "why": "w", "one_way": true}),
        )
        .unwrap();
        assert!(runs_here(&step));
        assert_eq!(
            text(&hint(&step, 0, 1, 80)),
            "Answer “Sign in?”: say how it went or ! run it · esc write instead"
        );
        // A task's step is the task's to be told of: run here, it would
        // not see what ran.
        step.task = Some(Asker {
            bot: "calc.fix".into(),
            name: "fix".into(),
        });
        assert!(!runs_here(&step));
        assert_eq!(
            text(&hint(&step, 0, 1, 80)),
            "Answer “Sign in?” from fix: say how it went · esc write instead"
        );
        assert!(!runs_here(&call("c1", &keep_api()).unwrap()));
        // What ran and how it exited, its first line for a long one.
        let ran = |command: &str, exit| crate::user_shell::Ran {
            command: command.into(),
            exit,
            output: "Logged in".into(),
            seconds: 1.0,
        };
        assert_eq!(super::ran(&ran("npm login", 0)), "Ran ! npm login · exit 0");
        assert_eq!(
            super::ran(&ran("cd web\nnpm login", 1)),
            "Ran ! cd web … · exit 1"
        );
    }

    /// The hint for `ask` at `width` fits it and ends in the key that
    /// leaves it for words of one's own, by display width.
    fn fits(ask: &Ask, at: usize, of: usize, width: usize) -> String {
        let row = text(&hint(ask, at, of, width));
        assert!(row.width() <= width, "{width}: {row}");
        assert!(row.ends_with(" esc write instead"), "{width}: {row}");
        row
    }

    fn should() -> Ask {
        call(
            "c1",
            &json!({
                "question": "Should we keep the old API for one more release?",
                "why": "w",
                "options": [
                    {"label": "Keep it", "explanation": "e"},
                    {"label": "Drop it", "explanation": "e"}
                ]
            }),
        )
        .unwrap()
    }

    #[test]
    fn short_of_room_the_question_shortens_then_its_keys_then_its_words_go() {
        let ask = should();
        assert_eq!(
            fits(&ask, 0, 1, 96),
            "Answer “Should we keep the old API for one more release?”: type or 1–2 pick · esc write instead"
        );
        assert_eq!(
            fits(&ask, 0, 1, 56),
            "Answer “Should w…”: type or 1–2 pick · esc write instead"
        );
        // Then `1–2 pick` goes, and `type or` with it: one phrase.
        assert_eq!(fits(&ask, 0, 1, 53), "Answer “Should…”: esc write instead");
        assert_eq!(fits(&ask, 0, 1, 40), "Answer “Should…”: esc write instead");
        // Measured by display width: a wide character takes two.
        let mut wide = should();
        wide.question = "要不要再保留旧接口一个版本？".into();
        assert_eq!(
            fits(&wide, 0, 1, 56),
            "Answer “要不要再…”: type or 1–2 pick · esc write instead"
        );
    }

    #[test]
    fn two_open_the_switch_goes_before_the_pick() {
        let ask = should();
        assert_eq!(
            fits(&ask, 0, 2, 76),
            "Answer “Should…” (1 of 2): type or 1–2 pick · ↑↓ switch · esc write instead"
        );
        assert_eq!(
            fits(&ask, 0, 2, 74),
            "Answer “Should…” (1 of 2): type or 1–2 pick · esc write instead"
        );
        // Then the pick, the words, and which of the two it is.
        assert_eq!(
            fits(&ask, 0, 2, 51),
            "Answer “Should…” (1 of 2): esc write instead"
        );
        assert_eq!(fits(&ask, 0, 2, 40), "Answer “Should…”: esc write instead");
    }

    #[test]
    fn a_step_short_of_room_drops_how_it_went_before_its_key() {
        let mut step = call(
            "c1",
            &json!({"question": "Sign in to the registry with your token?", "why": "w"}),
        )
        .unwrap();
        assert_eq!(
            fits(&step, 0, 1, 40),
            "Answer “Sign in…”: esc write instead"
        );
        step.task = Some(Asker {
            bot: "calc.fix".into(),
            name: "fix".into(),
        });
        assert_eq!(
            fits(&step, 0, 1, 45),
            "Answer “Sign in…” from fix: esc write instead"
        );
        // Too short to keep three of its letters, the name goes whole.
        assert_eq!(
            fits(&step, 0, 1, 44),
            "Answer “Sign in…”: esc write instead"
        );
        assert_eq!(hint_name(&step, 0, 1, 44), None);
    }

    #[test]
    fn a_tasks_long_name_shortens_last_and_stays_its_link() {
        let mut ask = should();
        let name = "release-notes-for-version";
        assert_eq!(name.width(), 25);
        ask.task = Some(Asker {
            bot: "calc.notes".into(),
            name: name.into(),
        });
        assert_eq!(
            fits(&ask, 0, 2, 96),
            "Answer “Should…” from release-notes-for-version (1 of 2): type or 1–2 pick · esc write instead"
        );
        assert_eq!(hint_name(&ask, 0, 2, 96).as_deref(), Some(name));
        assert_eq!(
            fits(&ask, 0, 2, 66),
            "Answer “Should…” from release-notes-for-version: esc write instead"
        );
        assert_eq!(
            fits(&ask, 0, 2, 55),
            "Answer “Should…” from release-notes…: esc write instead"
        );
        assert_eq!(hint_name(&ask, 0, 2, 55).as_deref(), Some("release-notes…"));
        // While three of its letters show; short of that it goes whole,
        // with its `from` and its link. The card says whose it is.
        assert_eq!(
            fits(&ask, 0, 2, 45),
            "Answer “Should…” from rel…: esc write instead"
        );
        assert_eq!(hint_name(&ask, 0, 2, 45).as_deref(), Some("rel…"));
        for width in [44, 40] {
            assert_eq!(
                fits(&ask, 0, 2, width),
                "Answer “Should…”: esc write instead"
            );
            assert_eq!(hint_name(&ask, 0, 2, width), None);
        }
        // Narrower than that row, the question shortens past its 8
        // columns.
        assert_eq!(fits(&ask, 0, 2, 33), "Answer “Shou…”: esc write instead");
    }

    #[test]
    fn the_hint_gives_way_in_order_a_later_part_only_once_the_earlier_have() {
        let named = |mut ask: Ask, name: &str| {
            ask.task = Some(Asker {
                bot: "calc.x".into(),
                name: name.into(),
            });
            ask
        };
        let step = call(
            "c2",
            &json!({"question": "Sign in to the registry?", "why": "w"}),
        );
        let asks = [
            should(),
            named(should(), "release-notes-for-version"),
            step.clone().unwrap(),
            named(step.unwrap(), "fix"),
        ];
        for ask in &asks {
            for of in [1, 2, 3] {
                let whole = fit_hint(ask, 0, of, 1000);
                for width in 20..=140 {
                    let fitted = fit_hint(ask, 0, of, width);
                    // Each part whole or not, by the order it gives way in;
                    // one with nothing to give has no say.
                    let parts = [
                        Some(fitted.question == whole.question),
                        (whole.keys != "esc write instead").then(|| fitted.keys == whole.keys),
                        Some(fitted.words == whole.words),
                        (!whole.place.is_empty()).then(|| fitted.place == whole.place),
                        whole.name.as_ref().map(|_| fitted.name == whole.name),
                    ];
                    let parts: Vec<bool> = parts.into_iter().flatten().collect();
                    for (at, &kept) in parts.iter().enumerate() {
                        assert!(
                            !kept || parts[at + 1..].iter().all(|&later| later),
                            "{width}: {}",
                            text(&hint(ask, 0, of, width))
                        );
                    }
                    // Whatever is cut ends in `…`, and a key is whole. A
                    // name shows three letters or none, and `type or` goes
                    // with the pick.
                    let row = text(&hint(ask, 0, of, width));
                    assert!(row.width() <= width, "{width}: {row}");
                    assert!(!row.contains("type or esc"), "{width}: {row}");
                    assert!(!row.contains("from …"), "{width}: {row}");
                    if let Some(name) = &fitted.name {
                        let letters = name.trim_end_matches('…').chars().count();
                        assert!(Some(name) == whole.name.as_ref() || letters >= 3, "{row}");
                    }
                    if width >= 40 {
                        assert!(row.ends_with(" esc write instead"), "{width}: {row}");
                    }
                    if fitted.question != whole.question {
                        assert!(fitted.question.ends_with('…'));
                    }
                }
            }
        }
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
        // No options: a step of the person's, said or run.
        let open = call("c2", &json!({"question": "Why?"})).unwrap();
        assert_eq!(
            text(&hint(&open, 0, 1, 80)),
            "Answer “Why?”: say how it went or ! run it · esc write instead"
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
        // Words of the person's own after it leave it open: only a pick
        // or the agent's close settles it.
        assert_eq!(
            open(&[person("Meanwhile, why?"), reply.clone(), asked.clone()]),
            Some("Keep it?".into())
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
                "recommended": 2
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

    fn asked(one_way: bool) -> Value {
        let mut arguments = keep_api();
        arguments["one_way"] = json!(one_way);
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
                "arguments": asked(true).to_string()})
        };
        let refused = json!({"type": "function_call_output", "call_id": "a3", "failed": true,
            "output": "invalid_arguments"});
        let ask = call(&id(2, "a1"), &asked(true)).unwrap();
        let person = |text: &str| {
            json!({"node": 9, "from": null, "origin": null,
            "item": {"role": "user", "content": [{"type": "input_text", "text": text}]}})
        };
        let taken = |call: &str| {
            json!({"type": "function_call_output", "call_id": call,
                "output": json!({"id": call, STAYS_OPEN: true}).to_string()})
        };
        // Newest first: a fourth not taken yet and a refused third, which
        // put nothing yet; words past the second, which leave it open, an
        // answer to the first.
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
                ("ask:3/a2".to_owned(), State::Open),
                ("ask:2/a1".to_owned(), State::Answered("yes".into()))
            ]
        );
    }

    /// A question asked before cards stayed open until a pick or a close
    /// was set aside by the person's own words after it, as every window
    /// showed it then: every reader reads it so still, and does not raise
    /// it again. One asked since, its result saying so, stays open past
    /// their words. Reproduces George's report of 11-27: after the upgrade,
    /// every old question answered in words came back open.
    #[test]
    fn a_question_asked_before_cards_stayed_open_is_set_aside_by_words_after_it() {
        let result = |stays_open: bool| {
            let mut said = json!({"id": "ask:2/a1", "if_unanswered": "wait"});
            if stays_open {
                said[STAYS_OPEN] = json!(true);
            }
            json!({"type": "function_call_output", "call_id": "a1", "turn": 2,
                "output": said.to_string()})
        };
        let asking = json!({"type": "function_call", "name": TOOL, "call_id": "a1", "turn": 2,
            "arguments": asked(true).to_string()});
        let words = |text: &str| {
            json!({"role": "user", "turn": 3,
                "content": [{"type": "input_text", "text": text}]})
        };
        let ask = call(&id(2, "a1"), &asked(true)).unwrap();
        // The window's question and the count, newest first.
        let past = |stays_open: bool| {
            open(&[
                words("Hold on, ship it Monday"),
                result(stays_open),
                asking.clone(),
            ])
        };
        assert_eq!(past(false), None, "set aside, as it was then");
        assert!(
            past(true).is_some_and(|open| open.stays_open),
            "asked since"
        );
        assert!(
            open(&[result(false), asking.clone()]).is_some_and(|open| !open.stays_open),
            "no words after it: open, as it was then"
        );
        // Words of another agent's, a reaction or an answer to it set
        // nothing aside.
        let mut relayed = words("Build is green");
        relayed["from"] = json!("shop.fix");
        assert!(open(&[relayed, result(false), asking.clone()]).is_some());
        assert_eq!(
            open(&[words(&answer(&ask, "Now")), result(false), asking.clone()]),
            None
        );
        // Read whole, as the Slack route and `sonny ask list` read it:
        // set aside, a pick on it still answering it.
        let entry = |node: i64, item: &Value| json!({"node": node, "item": item});
        let person = |text: &str| {
            json!({"node": 9, "from": null, "origin": null,
            "item": {"role": "user", "content": [{"type": "input_text", "text": text}]}})
        };
        let stands = |stays_open: bool, entries: &[(i64, Value)]| {
            let mut reading = Asked::default();
            (entries.iter())
                .flat_map(|(at, entry)| reading.entry(*at, entry))
                .map(|(ask, state)| (ask.stays_open, state))
                .next()
                .filter(|(read, _)| *read == stays_open)
                .map(|(_, state)| state)
        };
        for stays_open in [false, true] {
            let entries = [
                (3, person("Hold on, ship it Monday")),
                (2, entry(2, &result(stays_open))),
                (2, entry(1, &asking)),
            ];
            let want = match stays_open {
                false => State::Aside,
                true => State::Open,
            };
            assert_eq!(stands(stays_open, &entries), Some(want));
            // Its deadline's gate reads it the same way.
            let mut reading = Standing::default();
            let found = (entries.iter()).find_map(|(at, entry)| reading.entry(*at, entry, 2, "a1"));
            assert_eq!(
                found.flatten().map(|(_, state)| state.settled()),
                Some(!stays_open)
            );
        }
        // Set aside, a pick still answers it.
        assert!(State::Aside.settled() && State::Aside.takes_a_pick());
    }

    #[test]
    fn an_agents_close_settles_its_ask_for_every_reader() {
        let asking = json!({"type": "function_call", "name": TOOL, "call_id": "a1", "turn": 2,
            "arguments": asked(true).to_string()});
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
        // The person's words past it leave it open; their answer stands
        // over any close.
        assert_eq!(
            stands(&[(4, person("hold on")), open_one.clone()]),
            Some(State::Open)
        );
        let ask = call(&id(2, "a1"), &asked(true)).unwrap();
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
        let other = call(&id(1, "z9"), &asked(true)).unwrap();
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
        let ask = call(&id(1, "c1"), &asked(true)).unwrap();
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
        // The agent says one thing: whether it is a one-way door. How long
        // the person waits is theirs to set (George, 11-21).
        let required = tool["parameters"]["required"].as_array().unwrap();
        assert_eq!(
            required,
            &[json!("question"), json!("why"), json!("one_way")]
        );
        let properties = tool["parameters"]["properties"].as_object().unwrap();
        assert_eq!(properties["one_way"]["type"], "boolean");
        for gone in ["if_unanswered", "wait_minutes", "command", "reason"] {
            assert!(!properties.contains_key(gone), "{gone}");
        }
        // Anything outward-facing waits, and a pick of theirs later wins.
        let one_way = properties["one_way"]["description"].as_str().unwrap();
        for words in ["in their name", "approves", "cannot be undone", "undo it"] {
            assert!(one_way.contains(words), "{words}");
        }
    }

    #[test]
    fn the_tool_says_what_it_takes_in_few_words() {
        // 338 words before George asked for a simpler tool (11-21): what
        // happens meanwhile is in the result, what a yes covers in covers.
        let description = tool()["description"].as_str().unwrap().to_owned();
        assert!(
            description.split_whitespace().count() <= 125,
            "{description}"
        );
    }

    #[test]
    fn what_the_agent_does_meanwhile_is_read_from_the_call_and_its_result() {
        assert_eq!(call("c", &keep_api()).unwrap().unanswered, None);
        assert_eq!(
            call("c", &asked(false)).unwrap().unanswered,
            Some(Unanswered::Later)
        );
        assert_eq!(
            call("c", &asked(true)).unwrap().unanswered,
            Some(Unanswered::Wait)
        );
        // Nothing recommended, nothing to go ahead with: it waits.
        let mut none = asked(false);
        none["recommended"] = json!(9);
        assert_eq!(call("c", &none).unwrap().unanswered, Some(Unanswered::Wait));
        // The result says how Sonny took it: deadlines off made it wait.
        let ask = call("c", &asked(false)).unwrap();
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
        let words = |plan: Unanswered, deadline: Option<i64>| {
            let mut ask = call("c", &asked(plan == Unanswered::Wait)).unwrap();
            ask.unanswered = Some(plan);
            ask.deadline = deadline;
            let said = meanwhile(&ask, now).unwrap();
            said.iter()
                .map(|(text, _)| text.as_str())
                .collect::<String>()
        };
        assert_eq!(
            words(Unanswered::Now, None),
            "Going with 2 meanwhile; another pick undoes it."
        );
        let at = now + 3600;
        assert_eq!(
            words(Unanswered::Later, Some(at)),
            format!("Goes with 2 at {} unless you pick.", clock(at, now))
        );
        assert_eq!(
            words(Unanswered::Wait, Some(at)),
            format!("Waits for your pick; a reminder at {}.", clock(at, now))
        );
        assert_eq!(words(Unanswered::Later, None), "Waits for your pick.");
        assert_eq!(words(Unanswered::Wait, None), "Waits for your pick.");
        // The time is at the text's own weight; the words around it muted.
        let mut ask = call("c", &asked(false)).unwrap();
        ask.deadline = Some(at);
        let weights: Vec<bool> = meanwhile(&ask, now).unwrap().iter().map(|w| w.1).collect();
        assert_eq!(weights, [false, true, false]);
        // An ask from before agents said draws no such row.
        assert_eq!(meanwhile(&call("c", &keep_api()).unwrap(), now), None);
        // On the card, under the options, barred.
        let card = card(
            &settled(
                call("c", &asked(false)).unwrap(),
                r#"{"if_unanswered":"go_now","deadline_s":null}"#,
            ),
            &State::Open,
            false,
            60,
        );
        let shown: Vec<String> = card.rows.iter().map(|row| row.to_string()).collect();
        assert_eq!(
            shown[shown.len() - 3..],
            [
                "┃    A smaller daemon; 0.3 users upgrade both.",
                "┃ ",
                "┃ Going with 2 meanwhile; another pick undoes it.",
            ]
        );
        assert_eq!(card.hangs.len(), card.rows.len());
    }

    fn sign_in() -> Value {
        json!({
            "question": "Sign in to the package registry",
            "why": "Publishing the release needs your account.\n\n```\nnpm login --auth-type=web\n```",
            "one_way": false,
        })
    }

    #[test]
    fn an_option_says_what_the_person_sees_or_gets_once() {
        // George, 11-19: picks on cards whose options said how a change
        // works left him asking what it does for him.
        let tool = tool();
        let explanation = tool["parameters"]["properties"]["options"]["items"]["properties"]
            ["explanation"]["description"]
            .as_str()
            .unwrap();
        // Sol, 11-20: told only to say what they see or get, every option
        // still said how it works ("a local SQLite database with
        // transactional saves"), so that goes to `why` by name.
        assert!(explanation.contains("What the person will see, keep, wait for or give up"));
        assert!(explanation.contains("in their words"));
        assert!(explanation.contains("why says how it works"));
        let why = tool["parameters"]["properties"]["why"]["description"]
            .as_str()
            .unwrap();
        assert!(why.contains("how each option works"));
        // Said there alone, not again in the tool's description.
        let description = tool["description"].as_str().unwrap();
        assert!(!description.contains("each option"));
    }

    #[test]
    fn a_step_only_the_person_can_take_is_an_ask_with_no_options() {
        let tool = tool();
        let required = tool["parameters"]["required"].as_array().unwrap();
        assert!(!required.contains(&json!("options")));
        assert!(
            tool["description"]
                .as_str()
                .unwrap()
                .contains("is an ask with no options, the exact line to run in why")
        );
        let ask = call(&id(4, "c9"), &sign_in()).unwrap();
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
        let hint: String = hint(&ask, 0, 1, 100)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            hint,
            "Answer “Sign in to the package registry”: say how it went or ! run it · esc write \
             instead"
        );
    }

    #[test]
    fn a_step_reads_its_line_to_run_in_its_why() {
        let mut ask = call("c9", &sign_in()).unwrap();
        ask.deadline = None;
        let card = card(&ask, &State::Open, false, 60);
        let shown: Vec<String> = (card.rows.iter())
            .map(|row| row.to_string().trim_end().to_owned())
            .collect();
        assert!(
            shown
                .iter()
                .any(|row| row.contains("npm login --auth-type=web")),
            "{shown:?}"
        );
        assert_eq!(shown.last().unwrap(), "┃ Waits for you to run it.");
        assert!(card.options.is_empty());
        assert_eq!(card.hangs.len(), card.rows.len());
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
        asked["why"] = json!(
            "Writes the notes.\n\n\n```\ncat <<'EOF' > notes.txt\nfirst  \n\n\nsecond\nEOF\n```  \n\n\nThen go on."
        );
        let ask = call("c9", &asked).unwrap();
        // Prose around it is tidied; the block is as written.
        assert_eq!(
            ask.why,
            "Writes the notes.\n\n```\ncat <<'EOF' > notes.txt\nfirst  \n\n\nsecond\nEOF\n```\n\nThen go on."
        );
        let shown: Vec<String> = (card(&ask, &State::Open, false, 60).rows.iter())
            .map(|row| row.to_string())
            .collect();
        let first = shown.iter().position(|row| row.contains("first")).unwrap();
        assert!(shown[first + 3].contains("second"), "{shown:?}");
    }

    #[test]
    fn an_ask_says_whether_it_is_a_one_way_door() {
        let mut unsaid = keep_api();
        let refused = served(
            &unsaid,
            "shop.fix",
            1,
            "c1",
            &crate::trigger::Shell::default(),
        )
        .unwrap_err();
        assert_eq!(refused, "invalid_arguments: one_way is true or false");
        // Going ahead goes ahead with the option recommended.
        unsaid["one_way"] = json!(false);
        unsaid.as_object_mut().unwrap().remove("recommended");
        let refused = served(
            &unsaid,
            "shop.fix",
            1,
            "c1",
            &crate::trigger::Shell::default(),
        )
        .unwrap_err();
        assert!(refused.contains("in recommended"), "{refused}");
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
        let later = fire(
            "Keep the old API?\nmore",
            Unanswered::Later,
            Some(1),
            "Drop it",
            false,
        );
        assert!(later.contains("\"Keep the old API?\""));
        assert!(later.contains("go on now with option 2 \"Drop it\""));
        assert!(later.contains("undo it and follow their pick"));
        assert!(later.contains("This is Sonny, not the person"));
        let wait = fire("Tag it?", Unanswered::Wait, None, "", false);
        assert!(wait.contains("do not go ahead with it"));
        assert!(wait.contains("This is Sonny, not the person"));
        let step = fire("Sign in to npm", Unanswered::Wait, None, "", true);
        assert!(step.contains("your step went: \"Sign in to npm\""));
        assert!(step.contains("do not go ahead with what needs it"));
        // The card, the composer and the pane already show an open ask
        // (George, 11-19): a reminder wakes the agent without asking it
        // for a line about what is held up.
        for fired in [&wait, &step] {
            assert!(fired.contains("Go on with what does not depend on it. This is Sonny"));
            assert!(!fired.contains("one line"));
        }
    }

    #[test]
    fn a_reminder_reads_back_what_its_fire_says_and_says_it_as_the_card_does() {
        let gate = "'/opt/sonny' ask open 'tally.halves' 10 'toolu_01'";
        let when = "at 2026-11-15 01:55";
        let at = crate::trigger::epoch_of(crate::trigger::Local {
            year: 2026,
            month: 11,
            day: 15,
            hour: 1,
            minute: 55,
        })
        .unwrap();
        let question = "Round half cents up, or to even?";
        let later = fire(question, Unanswered::Later, Some(0), "Up", false);
        let read = reminder(gate, when, &later).unwrap();
        assert_eq!(
            read,
            Reminder {
                asker: "tally.halves".into(),
                question: question.into(),
                plan: Unanswered::Later,
                pick: Some(0),
                step: false,
                at,
            }
        );
        let said = |r: &Reminder| {
            r.says(at - 3600)
                .into_iter()
                .map(|(t, _)| t)
                .collect::<String>()
        };
        assert_eq!(said(&read), "Goes with 1 at 01:55 unless you pick.");
        let wait = reminder(
            gate,
            when,
            &fire(question, Unanswered::Wait, None, "", false),
        )
        .unwrap();
        assert_eq!(
            (wait.plan, wait.pick, wait.step),
            (Unanswered::Wait, None, false)
        );
        assert_eq!(said(&wait), "Waits for your pick; a reminder at 01:55.");
        let step = reminder(
            gate,
            when,
            &fire("npm login", Unanswered::Wait, None, "", true),
        )
        .unwrap();
        assert_eq!((step.question.as_str(), step.step), ("npm login", true));
        assert_eq!(said(&step), "Waits for you to run it; a reminder at 01:55.");
        // A reminder set before its fire dropped the line about what is
        // held up reads back the same.
        let old = format!(
            "{WAIT}{question}{WAIT_WAITS} person, so do not go ahead with it. Go on with what \
             does not depend on it, and say in one line what it holds up, so they see it again."
        );
        assert_eq!(reminder(gate, when, &old), Some(wait.clone()));
        // A name with a quote in it, as the gate writes it.
        let quoted = reminder("'/s' ask open 'it'\\''s' 3 'c'", when, &later).unwrap();
        assert_eq!(quoted.asker, "it's");
        // Not a question's: another `--if`, another time, another message.
        assert!(reminder("'/s' changed", when, &later).is_none());
        assert!(reminder(gate, "every 30m", &later).is_none());
        assert!(reminder(gate, when, "Check the build.").is_none());
    }

    #[test]
    fn an_ask_whose_call_failed_reminds_of_nothing() {
        let asking = json!({"node": 1, "item": {"type": "function_call", "name": TOOL,
            "call_id": "a1", "arguments": asked(true).to_string()}});
        let result = |failed: bool, output: &str| {
            json!({"node": 2, "item": {"type": "function_call_output", "call_id": "a1",
                "failed": failed, "output": output}})
        };
        let read = |entries: &[Value]| {
            let mut reading = Reading::default();
            (entries.iter()).find_map(|entry| reading.entry(2, entry, 2, "a1"))
        };
        // Taken, or still running, it waits on the person.
        let taken = result(false, r#"{"id":"ask:2/a1"}"#);
        assert_eq!(read(&[taken, asking.clone()]), Some(true));
        assert_eq!(read(std::slice::from_ref(&asking)), Some(true));
        // Failed or refused, no card shows it: its deadline has nothing to
        // remind of, and a close finds no question there.
        let failed = result(true, "expired");
        assert_eq!(read(&[failed.clone(), asking.clone()]), Some(false));
        let refused = result(false, r#"{"error":"denied"}"#);
        assert_eq!(read(&[refused, asking.clone()]), Some(false));
        let mut standing = Standing::default();
        assert_eq!(
            [failed, asking.clone()]
                .iter()
                .find_map(|entry| standing.entry(2, entry, 2, "a1")),
            Some(None)
        );
        // Another turn's failed call of the same ID is not this one.
        let mut reading = Reading::default();
        let elsewhere = result(true, "expired");
        assert_eq!(
            [(3, elsewhere), (2, asking)]
                .iter()
                .find_map(|(at, entry)| reading.entry(*at, entry, 2, "a1")),
            Some(true)
        );
    }

    #[test]
    fn an_answered_or_closed_ask_lets_its_deadline_go() {
        assert_eq!(named("ask:12/toolu_1"), Some((12, "toolu_1")));
        assert_eq!(named("ask:12/"), None);
        assert_eq!(named("ask:x/toolu_1"), None);
        assert_eq!(named("toolu_1"), None);
        let places = crate::trigger::Places::home().unwrap();
        std::fs::create_dir_all(&places.jobs).unwrap();
        std::fs::create_dir_all(&places.state).unwrap();
        let job = places.job(&trigger_name("let.go", 12, "toolu_01AbCdEf"));
        std::fs::write(&job, "{}").unwrap();
        // Another ask's deadline stays.
        let other = places.job(&trigger_name("let.go", 13, "toolu_01AbCdEf"));
        std::fs::write(&other, "{}").unwrap();
        let_deadline_go("let.go", "ask:12/toolu_01AbCdEf");
        assert!(!job.exists());
        assert!(other.exists());
        // Gone already, or never set, is fine.
        let_deadline_go("let.go", "ask:12/toolu_01AbCdEf");
        let_deadline_go("let.go", "not an ask");
    }

    #[test]
    fn an_ask_stays_open_until_a_pick_or_the_agents_close() {
        let asking = json!({"node": 1, "item": {"type": "function_call", "name": TOOL,
            "call_id": "a1", "arguments": asked(false).to_string()}});
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
        // Words of their own after it change nothing: its deadline still
        // fires, as the agent did not close it.
        assert_eq!(
            read(&[person("Actually, hold on"), asking.clone()]),
            Some(true)
        );
        // Its own answer settles it; another question's does not.
        let ask = call(&id(2, "a1"), &asked(false)).unwrap();
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
        let earlier = call(&id(1, "a1"), &asked(false)).unwrap();
        assert_eq!(
            read(&[person(&answer(&earlier, "1")), asking.clone()]),
            Some(true)
        );
        // A later turn's ask reusing the call, answered too, is another
        // ask: the reading goes on past it to this one's answer.
        let mut reused = asking.clone();
        reused["turn"] = json!(5);
        let later = call(&id(5, "a1"), &asked(false)).unwrap();
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
        let again = call(&id(2, "a3"), &asked(false)).unwrap();
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
            let arguments = json!({"question": question, "why": "w", "one_way": true,
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
        let arguments = asked(false);
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

    /// A bot's history holding one ask, made with `arguments` and taken
    /// as `output` says, newest first as the daemon pages it.
    async fn history(arguments: Value, output: Value) -> std::sync::Arc<agent_client::Client> {
        let call = json!({"node": 1, "item": {"type": "function_call", "call_id": "c1",
            "name": TOOL, "arguments": arguments.to_string()}});
        let result = json!({"node": 2, "item": {"type": "function_call_output",
            "call_id": "c1", "output": output.to_string()}});
        crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => {
                json!({"nodes": [{"node": 2, "turn": 4}, {"node": 1, "turn": 4}]})
            }
            _ => json!({"items": [result.clone(), call.clone()]}),
        })
        .await
    }

    #[tokio::test]
    async fn a_bot_waits_on_an_open_ask_until_it_goes_ahead_by_itself() {
        let question = |one_way: bool| {
            json!({"question": "Ship?", "why": "w", "one_way": one_way,
                "options": [{"label": "Yes"}, {"label": "No"}], "recommended": 1})
        };
        let now = 1_000;
        let waits = |arguments: Value, output: Value| async move {
            let client = history(arguments, output).await;
            waits(&client, "shop.w.1", now).await.unwrap()
        };
        // Until the person answers.
        let wait = json!({"if_unanswered": "wait"});
        assert!(waits(question(true), wait).await);
        // Until it goes ahead.
        let later = |at: i64| json!({"if_unanswered": "go_later", "deadline_s": at});
        assert!(waits(question(false), later(now + 60)).await);
        assert!(!waits(question(false), later(now - 60)).await);
        // Not at all.
        assert!(!waits(question(false), json!({"if_unanswered": "go_now"})).await);
        // A step of the person's waits for them.
        let step = json!({"question": "Sign in?", "why": "w", "one_way": false});
        assert!(waits(step, json!({"if_unanswered": "wait"})).await);
    }

    /// A question stays open however much the agent did after it: one
    /// asked 600 nodes back still waits on the person.
    #[tokio::test]
    async fn a_question_under_a_long_run_of_work_still_waits() {
        const NEWEST: i64 = 602;
        let arguments = json!({"question": "Ship?", "why": "w", "one_way": true,
            "options": [{"label": "Yes"}, {"label": "No"}], "recommended": 1});
        let client = crate::tests::daemon_answering(move |asked| match asked["op"].as_str() {
            Some("history_nodes") => {
                let start = asked["from"].as_i64().unwrap_or(NEWEST);
                let least = (start - asked["limit"].as_i64().unwrap() + 1).max(1);
                let nodes: Vec<Value> = (least..=start)
                    .rev()
                    .map(|node| json!({"node": node, "turn": 4}))
                    .collect();
                let from = if least > 1 {
                    json!(least - 1)
                } else {
                    Value::Null
                };
                json!({"nodes": nodes, "next_from": from})
            }
            _ => {
                let items: Vec<Value> = (asked["nodes"].as_array().into_iter().flatten())
                    .filter_map(Value::as_i64)
                    .map(|node| match node {
                        1 => json!({"node": 1, "item": {"type": "function_call",
                            "call_id": "c1", "name": TOOL, "arguments": arguments.to_string()}}),
                        2 => json!({"node": 2, "item": {"type": "function_call_output",
                            "call_id": "c1",
                            "output": json!({"if_unanswered": "wait"}).to_string()}}),
                        _ => json!({"node": node, "item": {"role": "assistant",
                            "content": [{"type": "output_text", "text": "Working."}]}}),
                    })
                    .collect();
                json!({"items": items})
            }
        })
        .await;
        assert!(waits(&client, "shop.w.1", 1_000).await.unwrap());
    }
}
