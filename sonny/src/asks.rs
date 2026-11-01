//! Decisions for the person: the lead calls the `ask` tool Sonny declares
//! on it, which the daemon records, and Sonny draws the question in the
//! conversation where it was asked: why it is asked, each option with what
//! picking it means, the one the lead recommends and why, and what the
//! agent does while nobody answers. A pick (a click on an option, or its
//! number typed) answers it, and goes to the lead as their next message.
//! Words the person types are their own, never framed as an answer: they
//! may be meant for anything, and the agent reads whether they answer, as
//! a reply under a card in a chat is read. The open questions are read
//! from the history Sonny replays: its `ask` calls not yet answered.
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
/// Options are picked by one digit.
const OPTIONS: usize = 9;
/// The mark a decision starts with, and the bar down its left side.
const GLYPH: &str = "? ";
const BAR: &str = "┃ ";
/// The longest wait an agent may set: a week.
const MAX_MINUTES: u64 = 7 * 24 * 60;
/// How much of the question a fire's message quotes, in characters.
const QUOTED: usize = 300;

#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    /// The call that asked it.
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
}

/// What an agent does while the person has not answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub label: String,
    /// What picking it means.
    pub explanation: String,
}

/// The `ask` tool as Sonny declares it on a bot, answered by its tool
/// host, which sets the deadline.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Put a decision in front of the person: a fork in the road, an architectural \
            choice, or a step that cannot be undone, where their pick changes what you build. Sonny \
            shows it in the conversation where you ask it: the question, why you ask it, each option \
            with what happens if they pick it, the one you recommend and why, and what you do until \
            they answer. Options without what they mean are useless to the person, so say what each \
            one does to their goal. Ask once, one decision per call, and do not repeat it in your \
            reply. A pick comes as their next message, starting \"You asked:\" and your \
            question; words they write instead come as they wrote them, so read whether they \
            answer it. Say in if_unanswered what you do until then. go_now goes on with the option \
            you recommend at once, for a step you can undo once they have seen it. go_later goes \
            on with it once the wait passes with no word from them, for a step you can undo but \
            would rather they saw first. wait does nothing they have not picked: anything that goes \
            out in their name or to other people, approves, merges, publishes, deletes, spends or \
            cannot be undone waits, and so does an ask you are unsure of or one they said to wait \
            on. Sonny wakes you at the deadline unless they answered or wrote to you since; that \
            wake is Sonny's, never their answer, so it allows nothing that needs their own words. \
            If they pick another option after you went ahead, undo what you did and follow their \
            pick. Meanwhile do work that does not depend on the answer, or end your turn. A step \
            only the person can take, such as a sign-in or a command that needs their own \
            terminal and keyboard, is an ask too: give no options, put the exact line to run in \
            command, say in why what it unblocks, and wait; their next message says how it went. \
            Any other question with nothing to choose between goes in your reply instead.",
        "parameters": {
            "type": "object",
            "properties": {
                "question": {"type": "string",
                    "description": "The question, verb first, in the person's own words, in about twelve words."},
                "why": {"type": "string",
                    "description": "Why you ask now: what the answer decides and the background the person needs to choose, in a sentence or three."},
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
                    "description": "For a step only the person can take, in place of options: the \
                        exact line for them to paste into their own terminal, whole, with nothing \
                        to fill in."},
                "recommended": {"type": "integer",
                    "description": "The number of the option you recommend, from 1; with options."},
                "reason": {"type": "string",
                    "description": "Why you recommend it, in one sentence; with options."},
                "if_unanswered": {"type": "string", "enum": ["go_now", "go_later", "wait"],
                    "description": "What you do until they answer, as above."},
                "wait_minutes": {"type": "integer", "minimum": 1, "maximum": MAX_MINUTES,
                    "description": "How long go_later waits before you go ahead, or wait before you \
                        are reminded, when the work runs on a clock, such as a meeting or a release; \
                        left out, the person's setting."}
            },
            "required": ["question", "why", "if_unanswered"],
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

/// The question an `ask` call with `arguments` put, as call `id`.
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

/// The oldest question still open among a bot's `items`, newest first:
/// an `ask` call that was taken, with no answer to it after, as the pinned
/// questions are closed: by the person's `You asked:` message naming it,
/// one answer to one question, or by words of their own after it, the
/// conversation going on past it.
pub fn open(items: &[Value]) -> Option<String> {
    let (mut answers, mut refused, mut open) = (Vec::new(), Vec::new(), None);
    let mut moved_on = false;
    for item in items {
        if item["role"] == "user" {
            let text = crate::text(item);
            match answered(&text) {
                Some(question) => answers.push(question.to_owned()),
                None => moved_on |= moves_on(&text, item["from"].as_str()),
            }
        }
        if item["type"] == "function_call_output"
            && (item["failed"] == true || crate::reactions::refused(item))
        {
            refused.push(item["call_id"].clone());
        }
        if item["type"] == "function_call"
            && item["name"] == TOOL
            && !refused.contains(&item["call_id"])
            && let Some(ask) = item["arguments"]
                .as_str()
                .and_then(|a| serde_json::from_str::<Value>(a).ok())
                .and_then(|arguments| call("", &arguments))
        {
            match answers.iter().position(|q| *q == ask.question) {
                Some(at) => {
                    answers.remove(at);
                }
                None if !moved_on => open = Some(ask.question),
                None => {}
            }
        }
    }
    open
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

/// The question a person's message answers, when [`answer`] wrote it.
pub fn answered(text: &str) -> Option<&str> {
    split(text).map(|(question, _)| question)
}

/// A person's message [`answer`] wrote, as the question and its answer.
pub fn split(text: &str) -> Option<(&str, &str)> {
    text.strip_prefix("You asked: ")?.split_once("\n\n")
}

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

/// `asks` without the first one `text` answers; whether there was one.
pub fn close(asks: &mut Vec<Ask>, text: &str) -> bool {
    let at =
        answered(text).and_then(|question| asks.iter().position(|ask| ask.question == question));
    at.map(|at| asks.remove(at)).is_some()
}

/// Whether what is typed picks one of `ask`'s options: its number alone.
/// Only a pick answers a question from the composer; words are the
/// person's own.
pub fn picks(ask: &Ask, typed: &str) -> bool {
    (typed.trim().parse::<usize>()).is_ok_and(|n| (1..=ask.options.len()).contains(&n))
}

/// The person's message answering `ask`: an option's number stands for
/// the option.
pub fn answer(ask: &Ask, typed: &str) -> String {
    let picked = typed
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|at| ask.options.get(at))
        .map_or(typed, |choice| choice.label.as_str());
    format!("You asked: {}\n\n{picked}", ask.question)
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
}

/// The decision as it is drawn where it was asked: the question, why,
/// each option by number with what it means, and the recommendation.
/// A `?` in yellow, what needs the person, starts it, and a muted bar
/// down its left side sets it apart from the replies around it, as a
/// callout's does: a shape of its own without color too.
pub fn card(ask: &Ask, width: usize) -> Card {
    // Prose reads at most so wide, as a reply does, labels too.
    let width = width.min(crate::PROSE);
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
            for span in
                (row.spans.iter_mut()).filter(|span| ![BAR, GLYPH].contains(&&*span.content))
            {
                span.style = style.patch(span.style);
            }
            let hang = hang(&row, lead);
            (row, hang)
        })
    };
    let mut rows: Vec<(Line<'static>, Line<'static>)> = Vec::new();
    let spacer = || (Line::from(indent()), Line::from(indent()));
    rows.extend(markdown(
        &ask.question,
        Line::from(Span::raw(GLYPH).fg(Color::Yellow).bold()),
        Line::from(indent()),
        Style::new().bold(),
    ));
    rows.extend(markdown(
        &ask.why,
        Line::from(indent()),
        Line::from(indent()),
        Style::new().dim(),
    ));
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
    // follows, muted as the reason is, the time at the text's own weight.
    if let Some(meanwhile) = meanwhile(ask, now()) {
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
    }
}

/// The lead a card's `row` goes on under when wrapped again: its bar,
/// then room past the `lead` columns to its text, a list's marker or a
/// quote's bar being lead too, the quote's bar kept.
fn hang(row: &Line<'static>, lead: usize) -> Line<'static> {
    let text: String = row.spans.iter().map(|span| span.content.as_ref()).collect();
    let after: String = text.chars().skip(lead).collect();
    let spaces = after.len() - after.trim_start_matches(' ').len();
    let rest = &after[spaces..];
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let marker = match rest {
        _ if ["- ", "* ", "+ ", "• "].iter().any(|m| rest.starts_with(m)) => 2,
        _ if digits > 0 && [". ", ") "].iter().any(|m| rest[digits..].starts_with(m)) => digits + 2,
        _ => 0,
    };
    let mut spans = vec![
        Span::raw(BAR).dim(),
        Span::raw(" ".repeat(lead - BAR.width())),
    ];
    // Under a quote its bar goes on too.
    match rest.strip_prefix("▌ ") {
        Some(_) => spans.extend([Span::raw(" ".repeat(spaces)), Span::raw("▌ ").dim()]),
        None => spans.push(Span::raw(" ".repeat(spaces + marker))),
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
fn clock(epoch: i64, now: i64) -> String {
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
    let ask =
        self::call(call, arguments).ok_or("invalid_arguments: a question to ask, in question")?;
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
        json!({"if_unanswered": plan.name(), "deadline_s": deadline, "deadline": when,
        "deadline_error": unset, "next": next})
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
    let add = json!({"add": {
        "name": trigger_name(bot, call),
        "at": format!("{}-{:02}-{:02} {:02}:{:02}", at.year, at.month, at.day, at.hour, at.minute),
        "if": gate,
        "message": fire(&ask.question, plan, label),
    }});
    crate::trigger::tool(&add).map(|_| ())
}

/// The whole minute `minutes` from `now`, never before it, as a one-off
/// trigger keeps time.
fn deadline(minutes: u64, now: i64) -> i64 {
    let at = now + minutes as i64 * 60;
    at + (60 - at.rem_euclid(60)) % 60
}

/// The trigger an ask's deadline is, by its agent and call: a name the
/// person reads in `/background` beside the agent's own.
fn trigger_name(bot: &str, call: &str) -> String {
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
    format!("{bot}.ask-{tail}")
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

/// Whether ask call `call` is still open, read from the agent's history
/// newest first, one entry at a time, as [`open`] reads it: answered by
/// the person, or set aside by words of their own after it, it is
/// settled. `None` while the entries read have not reached the call.
#[derive(Default)]
struct Reading {
    /// The questions the person answered after the call, so far.
    answers: Vec<String>,
}

impl Reading {
    fn entry(&mut self, entry: &Value, call: &str) -> Option<bool> {
        if crate::lookout::said_by_person(entry).is_some() {
            let text = crate::text(&entry["item"]);
            match answered(&text) {
                Some(question) => self.answers.push(question.to_owned()),
                None if moves_on(&text, entry["from"].as_str()) => return Some(false),
                None => {}
            }
        }
        let asked = crate::responses(&entry["item"])
            .into_iter()
            .find(|item| item["type"] == "function_call" && item["call_id"] == call)?;
        let question = (asked["arguments"].as_str())
            .and_then(|a| serde_json::from_str::<Value>(a).ok())
            .and_then(|arguments| self::call("", &arguments))
            .map(|ask| ask.question);
        Some(question.is_some_and(|question| !self.answers.contains(&question)))
    }
}

/// Whether the ask is still open, read from the daemon this shell reaches.
pub async fn still_open(bot: &str, turn: i64, call: &str) -> Result<bool, String> {
    let daemon = crate::trigger::Daemon::current()?;
    let socket = daemon.socket()?;
    let client = crate::trigger::connect(&socket, &daemon).await?;
    let (mut reading, mut found) = (Reading::default(), None);
    crate::said::walk(&client, bot, turn, |_, entry| {
        found = reading.entry(entry, call);
        found.is_some()
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(found == Some(true))
}

/// `sonny ask open BOT TURN CALL`, an ask's trigger's `--if`: exit 0
/// while the ask is open, 1 once it is settled or cannot be read.
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
        Err(error) => {
            eprintln!("{error}");
            1
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
    keys.push("esc set aside".into());
    let keys = keys.join(" · ");
    let words = match ask.options.len() {
        0 if ask.command.is_some() => " say how it went ·",
        0 => " type your answer ·",
        _ => "",
    };
    let place = place(at, of);
    let room = width.saturating_sub(
        "Answer “”: ".chars().count()
            + place.chars().count()
            + words.chars().count()
            + keys.chars().count(),
    );
    let question = crate::transcript::cut(&plain(&ask.question), room.max(8));
    let mut spans = vec![Span::raw(format!("Answer “{question}”{place}:{words} ")).dim()];
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
        let ask = call("c1", &keep_api()).unwrap();
        assert_eq!(ask.question, "Keep the old API?");
        assert_eq!(
            ask.options
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["Keep it", "Drop it"]
        );
        assert_eq!(ask.recommended, Some(1));
        assert_eq!(call("c2", &json!({"question": " "})), None);
        // A recommendation past the options recommends none.
        let past = call("c2", &json!({"question": "Tag it?", "recommended": 3})).unwrap();
        assert_eq!(past.recommended, None);
        let mut open = vec![ask.clone(), past];
        close(&mut open, "how is it going?");
        assert_eq!(open.len(), 2);
        close(&mut open, &answer(&ask, "2"));
        assert_eq!(
            open.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["c2"]
        );
        // A question with a blank line in it is still closed by its answer.
        let long = call("c4", &json!({"question": "Ship it?\n\n  Tests pass.  "})).unwrap();
        assert_eq!(long.question, "Ship it?\n  Tests pass.");
        open.push(long.clone());
        close(&mut open, &answer(&long, "yes"));
        assert_eq!(open.len(), 1);
    }

    #[test]
    fn a_number_answers_with_its_option() {
        let ask = call("c1", &keep_api()).unwrap();
        assert_eq!(answer(&ask, "2"), "You asked: Keep the old API?\n\nDrop it");
        assert_eq!(
            answer(&ask, "3"),
            "You asked: Keep the old API?\n\n3",
            "no third option: the digit is the answer"
        );
        assert_eq!(
            answer(&ask, "Keep it for one release"),
            "You asked: Keep the old API?\n\nKeep it for one release"
        );
    }

    #[test]
    fn the_decision_reads_as_question_why_options_and_recommendation() {
        let ask = call("c1", &keep_api()).unwrap();
        let Card { rows, options, .. } = card(&ask, 60);
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
        let narrow: Vec<String> = card(&ask, 30)
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
    fn what_the_agent_wrote_in_a_decision_reads_as_markdown() {
        let ask = call(
            "c1",
            &json!({
                "question": "Ship **0.5** with `--fast` on?",
                "options": [{"label": "Yes", "explanation": "Runs $n^2$ checks, see [the plan](https://x.dev)."}],
            }),
        )
        .unwrap();
        let shown: Vec<String> = card(&ask, 80)
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
        let question = &card(&ask, 80).rows[0];
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
            "Answer “Keep the old API?”: 1–2 pick · esc set aside"
        );
        assert_eq!(
            text(&hint(&ask, 1, 2, 80)),
            "Answer “Keep the old API?” (2 of 2): 1–2 pick · ↑↓ switch · esc set aside"
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
            "Answer “Why?”: type your answer · esc set aside"
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
        assert!(!moves_on("You asked: Keep it?\n\nYes", None));
        assert!(!moves_on("Done: tests pass", Some("tally.halves")));
        assert!(!moves_on(&crate::reactions::message("👍", None), None));
    }

    #[test]
    fn a_question_is_open_until_its_answer_lands() {
        let asked = json!({"type": "function_call", "name": TOOL, "call_id": "a1",
            "arguments": json!({"question": "Keep it?", "options": [{"label": "Yes", "explanation": "e"}]}).to_string()});
        let reply =
            json!({"role": "assistant", "content": [{"type": "output_text", "text": "Asked."}]});
        let output = json!({"type": "function_call_output", "call_id": "a1", "output": "Asked."});
        let person =
            |text: &str| json!({"role": "user", "content": [{"type": "input_text", "text": text}]});
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
            open(&[
                reply.clone(),
                person("You asked: Keep it?\n\nYes"),
                asked.clone()
            ]),
            None
        );
        assert_eq!(open(&[reply, output]), None);
        // Two asked, one answered: the other stays open.
        let other = json!({"type": "function_call", "name": TOOL, "call_id": "a2",
            "arguments": json!({"question": "Ship it?"}).to_string()});
        assert_eq!(
            open(&[person("You asked: Keep it?\n\nYes"), other, asked.clone()]),
            Some("Ship it?".into())
        );
        let unknown = json!({"type": "function_call_output", "call_id": "a1",
            "output": json!({"error": "unknown_tool"}).to_string()});
        assert_eq!(open(&[unknown, asked]), None);
    }

    /// The card's rows as a screen draws them at `width`, laid at `made`.
    fn drawn(ask: &Ask, made: usize, width: u16) -> Vec<String> {
        use ratatui::{buffer::Buffer, layout::Rect};
        let card = card(ask, made);
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
                let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
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
        let card = card(&settled(call("c", &asked("go_now")).unwrap(), ""), 60);
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
        let ask = call("c9", &sign_in()).unwrap();
        assert_eq!(ask.command.as_deref(), Some("npm login --auth-type=web"));
        assert!(ask.options.is_empty());
        // Nothing to go ahead with: it waits, whatever was asked.
        assert_eq!(ask.unanswered, Some(Unanswered::Wait));
        // Still open until the person says how it went, as any ask.
        let items = [
            json!({"type": "function_call", "name": TOOL, "call_id": "c9",
            "arguments": sign_in().to_string()}),
        ];
        assert_eq!(
            open(&items).as_deref(),
            Some("Sign in to the package registry")
        );
        assert_eq!(
            answer(&ask, "done, signed in"),
            "You asked: Sign in to the package registry\n\ndone, signed in"
        );
        let hint: String = hint(&ask, 0, 1, 80)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            hint,
            "Answer “Sign in to the package registry”: say how it went · esc set aside"
        );
    }

    #[test]
    fn the_card_shows_the_line_to_run_where_a_click_copies_it() {
        let mut ask = call("c9", &sign_in()).unwrap();
        ask.deadline = None;
        let card = card(&ask, 60);
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
        let card = card(&ask, 60);
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
            trigger_name("shop.lead", "toolu_01AbCdEfGhIjK"),
            "shop.lead.ask-dEfGhIjK"
        );
        assert_eq!(trigger_name("a b", "c-1"), "a-b.ask-c1");
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
        let read = |entries: &[Value]| {
            let mut reading = Reading::default();
            entries.iter().find_map(|entry| reading.entry(entry, "a1"))
        };
        assert_eq!(read(std::slice::from_ref(&asking)), Some(true));
        // Words of their own after it: the conversation moved on.
        assert_eq!(
            read(&[person("Actually, hold on"), asking.clone()]),
            Some(false)
        );
        // Its own answer settles it; another question's does not.
        let ask = call("a1", &asked("go_later")).unwrap();
        assert_eq!(
            read(&[person(&answer(&ask, "1")), asking.clone()]),
            Some(false)
        );
        assert_eq!(
            read(&[person("You asked: Tag it?\n\nYes"), asking.clone()]),
            Some(true)
        );
        // An automation's, another agent's or a reaction are not the person.
        let automation = json!({"node": 8, "from": null, "origin": "trigger",
            "item": {"role": "user", "content": [{"type": "input_text", "text": "look"}]}});
        let agent = json!({"node": 7, "from": "shop.lead", "origin": null,
            "item": {"role": "user", "content": [{"type": "input_text", "text": "carry on"}]}});
        let thumbs = person(&crate::reactions::message("👍", None));
        assert_eq!(
            read(&[automation, agent, thumbs, asking.clone()]),
            Some(true)
        );
        // Before the call is reached, nothing is known yet.
        assert_eq!(read(&[person("You asked: Tag it?\n\nYes")]), None);
    }
}
