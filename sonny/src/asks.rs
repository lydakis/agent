//! Questions for the person: the lead calls the `ask` tool Sonny declares
//! on it, which the daemon records and Sonny pins above the composer,
//! oldest first, until the person answers them one at a time. The answer
//! goes to the lead as the person's next message. The open questions are
//! read from the lead's history: its `ask` calls not yet answered.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};

/// The name of the tool the lead asks with.
pub const TOOL: &str = "ask";
/// Choices are picked by one digit.
const CHOICES: usize = 9;

#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    /// The call that asked it.
    pub id: String,
    pub question: String,
    pub choices: Vec<String>,
}

/// The `ask` tool as Sonny declares it on a bot.
pub fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Ask the person to decide something you cannot go on without. Sonny pins the \
            question above their composer until they answer, and the answer comes as their next \
            message, starting \"You asked:\" and your question. One question per call, with up to 9 \
            short choices when the answer is one of a few. Then end your turn, unless you have work \
            that does not depend on the answer.",
        "parameters": {
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "The question, as the person reads it."},
                "choices": {"type": "array", "items": {"type": "string"},
                    "description": "Up to 9 short answers the person picks from by number."}
            },
            "required": ["question"],
            "additionalProperties": false
        },
        "result": "Asked. Sonny shows the question to the person until they answer, and the answer comes as their next message. End your turn now, unless you have work that does not depend on it."
    })
}

/// The question an `ask` call with `arguments` put, as call `id`.
pub fn call(id: &str, arguments: &Value) -> Option<Ask> {
    // Without blank lines, so its answer still reads as answering it.
    let question = arguments["question"]
        .as_str()?
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let question = question.trim();
    let choices: Vec<String> = arguments["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|choice| choice.as_str().map(str::trim))
        .filter(|choice| !choice.is_empty())
        .map(str::to_owned)
        .take(CHOICES)
        .collect();
    (!question.is_empty()).then(|| Ask {
        id: id.to_owned(),
        question: question.to_owned(),
        choices,
    })
}

/// The question a person's message answers, when [`answer`] wrote it.
pub fn answered(text: &str) -> Option<&str> {
    let (question, _) = text.strip_prefix("You asked: ")?.split_once("\n\n")?;
    Some(question)
}

/// `asks` without the first one `text` answers.
pub fn close(asks: &mut Vec<Ask>, text: &str) {
    if let Some(question) = answered(text)
        && let Some(at) = asks.iter().position(|ask| ask.question == question)
    {
        asks.remove(at);
    }
}

/// The person's message answering `ask`: a choice's number stands for
/// the choice.
pub fn answer(ask: &Ask, typed: &str) -> String {
    let picked = typed
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|at| ask.choices.get(at))
        .map_or(typed, String::as_str);
    format!("You asked: {}\n\n{picked}", ask.question)
}

/// The panel: how many questions are open, then the first, and its
/// choices by number.
pub fn rows(asks: &[Ask], width: usize, chatting: bool) -> Vec<Line<'static>> {
    let Some(ask) = asks.first() else {
        return Vec::new();
    };
    let mut head = vec![
        Span::raw("? ").fg(Color::Yellow),
        Span::raw("Needs you").bold(),
    ];
    if asks.len() > 1 {
        head.push(Span::raw(format!(" · 1 of {}", asks.len())).dim());
    }
    if chatting {
        head.push(Span::raw(" · Esc to answer").dim());
    }
    // A blank row apart from the history above.
    let mut rows = vec![Line::default(), Line::from(head)];
    for line in ask.question.lines() {
        rows.extend(crate::transcript::wrap(
            Line::from(line.to_owned()),
            width,
            Span::raw("  "),
            Span::raw("  "),
        ));
    }
    if !ask.choices.is_empty() {
        let mut spans = Vec::new();
        for (at, choice) in ask.choices.iter().enumerate() {
            if at > 0 {
                spans.push(Span::raw(" · ").dim());
            }
            spans.push(Span::raw(format!("{} ", at + 1)).bold());
            spans.push(Span::raw(choice.clone()));
        }
        rows.extend(crate::transcript::wrap(
            Line::from(spans),
            width,
            Span::raw("  "),
            Span::raw("  "),
        ));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_puts_its_question_and_an_answer_closes_it() {
        let ask = call(
            "c1",
            &json!({"question": " Keep the old API? ",
            "choices": ["Keep it", " ", "Drop it"]}),
        )
        .unwrap();
        assert_eq!(ask.question, "Keep the old API?");
        assert_eq!(ask.choices, ["Keep it", "Drop it"]);
        assert_eq!(call("c2", &json!({"question": " "})), None);
        let mut open = vec![
            ask.clone(),
            call("c3", &json!({"question": "Tag it?"})).unwrap(),
        ];
        close(&mut open, "how is it going?");
        assert_eq!(open.len(), 2);
        close(&mut open, &answer(&ask, "2"));
        assert_eq!(
            open.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["c3"]
        );
        // A question with a blank line in it is still closed by its answer.
        let long = call("c4", &json!({"question": "Ship it?\n\n  Tests pass.  "})).unwrap();
        assert_eq!(long.question, "Ship it?\n  Tests pass.");
        open.push(long.clone());
        close(&mut open, &answer(&long, "yes"));
        assert_eq!(open.len(), 1);
    }

    #[test]
    fn a_number_answers_with_its_choice() {
        let ask = Ask {
            id: "c1".into(),
            question: "Keep the old API?".into(),
            choices: vec!["Keep it".into(), "Drop it".into()],
        };
        assert_eq!(answer(&ask, "2"), "You asked: Keep the old API?\n\nDrop it");
        assert_eq!(
            answer(&ask, "3"),
            "You asked: Keep the old API?\n\n3",
            "no third choice: the digit is the answer"
        );
        assert_eq!(
            answer(&ask, "Keep it for one release"),
            "You asked: Keep the old API?\n\nKeep it for one release"
        );
    }

    #[test]
    fn the_panel_shows_the_first_question_and_how_many_wait() {
        let ask = |question: &str| Ask {
            id: question.into(),
            question: question.into(),
            choices: vec!["Yes".into(), "No".into()],
        };
        let shown: Vec<String> = rows(&[ask("Ship it?"), ask("Tag it?")], 40, false)
            .iter()
            .map(|row| row.to_string())
            .collect();
        assert_eq!(
            shown,
            ["", "? Needs you · 1 of 2", "  Ship it?", "  1 Yes · 2 No"]
        );
    }
}
