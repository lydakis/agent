//! Decisions for the person: the lead calls the `ask` tool Sonny declares
//! on it, which the daemon records, and Sonny draws the question in the
//! conversation where it was asked: why it is asked, each option with what
//! picking it means, and the one the lead recommends and why. What the
//! person types next answers it, and goes to the lead as their next
//! message. The open questions are read from the history Sonny replays:
//! its `ask` calls not yet answered.
use ratatui::{
    style::{Color, Style, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};

/// The name of the tool the lead asks with.
pub const TOOL: &str = "ask";
/// Options are picked by one digit.
const OPTIONS: usize = 9;

#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    /// The call that asked it.
    pub id: String,
    pub question: String,
    /// Why it is asked: what the answer decides.
    pub why: String,
    pub options: Vec<Choice>,
    /// The option recommended, from 0, and why.
    pub recommended: Option<usize>,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub label: String,
    /// What picking it means.
    pub explanation: String,
}

/// The `ask` tool as Sonny declares it on a bot.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Put a decision in front of the person: a fork in the road, an architectural \
            choice, or a step that cannot be undone, where their pick changes what you build. Sonny \
            shows it in the conversation where you ask it: the question, why you ask it, each option \
            with what happens if they pick it, and the one you recommend and why. Options without \
            what they mean are useless to the person, so say what each one does to their goal. Ask \
            once, one decision per call, and do not repeat it in your reply. Their answer comes as \
            their next message, starting \"You asked:\" and your question. While you wait, go on \
            with the option you recommend if it can be undone once they have seen it, or with work \
            that does not depend on the answer; otherwise end your turn. A question with nothing to \
            choose between goes in your reply instead.",
        "parameters": {
            "type": "object",
            "properties": {
                "question": {"type": "string",
                    "description": "The question, verb first, in the person's own words, in about twelve words."},
                "why": {"type": "string",
                    "description": "Why you ask now: what the answer decides and the background the person needs to choose, in a sentence or three."},
                "options": {"type": "array",
                    "description": "2 to 4 options, each a different way forward.",
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
                "recommended": {"type": "integer",
                    "description": "The number of the option you recommend, from 1."},
                "reason": {"type": "string", "description": "Why you recommend it, in one sentence."}
            },
            "required": ["question", "why", "options", "recommended", "reason"],
            "additionalProperties": false
        },
        "result": "Asked. Sonny shows the decision to the person, and their answer comes as their next message. Go on with the option you recommend if it can be undone, or with work that does not depend on the answer; otherwise end your turn now."
    })
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
            explanation: tidy(option["explanation"].as_str()),
        })
        .filter(|choice| !choice.label.is_empty())
        .take(OPTIONS)
        .collect();
    let recommended = arguments["recommended"]
        .as_u64()
        .and_then(|n| (n as usize).checked_sub(1))
        .filter(|at| *at < options.len());
    (!question.is_empty()).then(|| Ask {
        id: id.to_owned(),
        question,
        why: tidy(arguments["why"].as_str()),
        options,
        recommended,
        reason: tidy(arguments["reason"].as_str()),
    })
}

/// The oldest question still open among a bot's `items`, newest first:
/// an `ask` call that was taken, with no answer to it after, as the pinned
/// questions are closed: only by the person's `You asked:` message naming
/// it, one answer to one question.
pub fn open(items: &[Value]) -> Option<String> {
    let (mut answers, mut refused, mut open) = (Vec::new(), Vec::new(), None);
    for item in items {
        if item["role"] == "user"
            && let Some(question) = answered(&crate::text(item))
        {
            answers.push(question.to_owned());
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
                None => open = Some(ask.question),
            }
        }
    }
    open
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
    let first = question.lines().next().unwrap_or("");
    let cut = crate::transcript::cut(first, width.saturating_sub(14).max(8));
    Line::from(format!("  answering “{cut}”")).dim()
}

/// `asks` without the first one `text` answers; whether there was one.
pub fn close(asks: &mut Vec<Ask>, text: &str) -> bool {
    let at =
        answered(text).and_then(|question| asks.iter().position(|ask| ask.question == question));
    at.map(|at| asks.remove(at)).is_some()
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

/// The decision as it is drawn where it was asked: the question, why,
/// each option by number with what it means, and the recommendation.
pub fn lines(ask: &Ask, width: usize) -> Vec<Line<'static>> {
    let indent = || Span::raw("  ");
    // What the agent wrote reads as its Markdown, as a reply does.
    let markdown = |text: &str, first: Line<'static>, rest: Line<'static>, style: Style| {
        let mut markdown = crate::transcript::Markdown::default();
        let rows = crate::transcript::markdown(&mut markdown, text, width, (first, rest));
        rows.into_iter().map(move |row| row.patch_style(style))
    };
    let mut rows = Vec::new();
    rows.extend(markdown(
        &ask.question,
        Line::from(Span::raw("? ").fg(Color::Yellow).bold()),
        Line::from(indent()),
        Style::new().bold(),
    ));
    rows.extend(markdown(
        &ask.why,
        Line::from(indent()),
        Line::from(indent()),
        Style::new().dim(),
    ));
    if !ask.options.is_empty() {
        rows.push(Line::default());
    }
    // The explanation sits under its label, past the number.
    let under = || Span::raw("     ");
    for (at, choice) in ask.options.iter().enumerate() {
        let mut label = vec![Span::raw(choice.label.clone()).bold()];
        if ask.recommended == Some(at) {
            label.push(Span::raw(" · recommended").fg(Color::Green));
        }
        rows.extend(crate::transcript::wrap_lines(
            Line::from(label),
            width,
            Line::from(vec![
                indent(),
                Span::raw(format!("{:<3}", at + 1)).fg(Color::Yellow),
            ]),
            Line::from(under()),
        ));
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
        rows.push(Line::default());
        rows.extend(markdown(
            &ask.reason,
            Line::from(vec![
                indent(),
                Span::raw(format!("Why {}: ", at + 1)).fg(Color::Green),
            ]),
            Line::from(indent()),
            Style::new().dim(),
        ));
    }
    rows
}

/// The composer's hint while what is typed answers `ask`, cut to `width`.
pub fn hint(ask: &Ask, more: usize, width: usize) -> String {
    let keys = match ask.options.len() {
        0 => "type your answer".to_owned(),
        1 => "1 or your own words".to_owned(),
        n => format!("1–{n} or your own words"),
    };
    let more = match more {
        0 => String::new(),
        n => format!(" · {n} more open"),
    };
    let tail = format!(": {keys}{more} · Esc to chat");
    let room = width.saturating_sub(tail.chars().count() + "Answer “”".chars().count());
    // The question as it reads on the card, without its Markdown marks.
    let question: String =
        crate::transcript::inline(ask.question.lines().next().unwrap_or(""), Style::default())
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
    format!(
        "Answer “{}”{tail}",
        crate::transcript::cut(&question, room.max(8))
    )
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
        let shown: Vec<String> = lines(&ask, 60).iter().map(|row| row.to_string()).collect();
        assert_eq!(
            shown,
            [
                "? Keep the old API?",
                "  The wire format changes, and 0.3 clients break.",
                "",
                "  1  Keep it",
                "     Old clients work; a shim stays a release.",
                "  2  Drop it · recommended",
                "     A smaller daemon; 0.3 users upgrade both.",
                "",
                "  Why 2: Nobody runs 0.3 outside this machine.",
            ]
        );
        // Narrow, each part wraps under its own indent.
        let narrow: Vec<String> = lines(&ask, 30)
            .iter()
            .map(|row| row.to_string().trim_end().to_owned())
            .collect();
        let at = narrow.iter().position(|row| row == "  1  Keep it").unwrap();
        assert_eq!(narrow[at + 1], "     Old clients work; a shim");
        assert_eq!(narrow[at + 2], "     stays a release.");
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
        let shown: Vec<String> = lines(&ask, 80).iter().map(|row| row.to_string()).collect();
        assert_eq!(shown[0], "? Ship 0.5 with --fast on?");
        assert_eq!(
            shown[3],
            "     Runs n² checks, see the plan (https://x.dev)."
        );
        // The question stays bold, past what its words make bold.
        assert!(
            lines(&ask, 80)[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
        // The composer names it without its marks.
        assert!(hint(&ask, 0, 80).starts_with("Answer “Ship 0.5 with --fast on?”"));
    }

    #[test]
    fn the_hint_names_the_question_and_its_keys() {
        let ask = call("c1", &keep_api()).unwrap();
        assert_eq!(
            hint(&ask, 0, 80),
            "Answer “Keep the old API?”: 1–2 or your own words · Esc to chat"
        );
        assert_eq!(
            hint(&ask, 1, 80),
            "Answer “Keep the old API?”: 1–2 or your own words · 1 more open · Esc to chat"
        );
        assert!(hint(&ask, 0, 50).contains('…'));
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
        // A reaction, or a message of the person's own, answers nothing.
        assert_eq!(
            open(&[
                person("👍"),
                person("Meanwhile, why?"),
                reply.clone(),
                asked.clone()
            ]),
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
}
