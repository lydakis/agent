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
use unicode_width::UnicodeWidthStr;

/// The name of the tool the lead asks with.
pub const TOOL: &str = "ask";
/// Options are picked by one digit.
const OPTIONS: usize = 9;
/// The mark a decision starts with, and the bar down its left side.
const GLYPH: &str = "? ";
const BAR: &str = "┃ ";

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
    let recommended = arguments["recommended"]
        .as_u64()
        .and_then(|n| (n as usize).checked_sub(1))
        .filter(|at| *at < options.len());
    (!question.is_empty()).then(|| Ask {
        id: id.to_owned(),
        question,
        why: prose(arguments["why"].as_str()),
        options,
        recommended,
        reason: prose(arguments["reason"].as_str()),
    })
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
    let (rows, hangs) = rows.into_iter().unzip();
    Card {
        rows,
        options,
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
    let words = match ask.options.len() {
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
            "Answer “Keep the old API?”: 1–2 pick · esc write instead"
        );
        assert_eq!(
            text(&hint(&ask, 1, 2, 80)),
            "Answer “Keep the old API?” (2 of 2): 1–2 pick · ↑↓ switch · esc write instead"
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
}
