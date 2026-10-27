//! Questions for the person: the lead runs `sonny ask` from its shell,
//! which leaves each question in a file of its own, and Sonny pins the open
//! ones above the composer, oldest first, until the person answers them one
//! at a time. The answer goes to the lead as the person's next message.
use ratatui::{
    style::{Color, Stylize},
    text::{Line, Span},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub const USAGE: &str = "usage: sonny ask QUESTION [--choice OPTION]...";
/// Choices are picked by one digit.
const CHOICES: usize = 9;

#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    pub file: PathBuf,
    pub question: String,
    pub choices: Vec<String>,
}

/// Where `bot`'s open questions are kept.
fn dir(bot: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".agent/sonny/asks").join(bot))
}

/// `sonny ask ...`, from the lead's shell.
pub fn run(args: &[String]) -> Result<(), String> {
    let bot = std::env::var("AGENT_BOT")
        .ok()
        .filter(|bot| !bot.is_empty() && !bot.contains('/'))
        .ok_or("run this from an agent's shell (AGENT_BOT is not set)")?;
    let (question, choices) = parse(args)?;
    let dir = dir(&bot).ok_or("HOME is not set")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    // Named to sort by when they were asked; written whole before Sonny
    // can see them, which skips names starting with a dot.
    let name = format!("{millis:015}-{}.json", std::process::id());
    let part = dir.join(format!(".{name}"));
    let body = json!({"question": question, "choices": choices}).to_string();
    std::fs::write(&part, body)
        .and_then(|()| std::fs::rename(&part, dir.join(&name)))
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    println!(
        "Asked. Sonny shows the question to the person until they answer, and the answer comes as their next message. End your turn now, unless you have work that does not depend on it."
    );
    Ok(())
}

fn parse(args: &[String]) -> Result<(String, Vec<String>), String> {
    let mut question = None;
    let mut choices = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--choice" => match args.next() {
                Some(choice) if !choice.trim().is_empty() => choices.push(choice.trim().to_owned()),
                _ => return Err(USAGE.into()),
            },
            _ if arg.starts_with("--") || question.is_some() || arg.trim().is_empty() => {
                return Err(USAGE.into());
            }
            _ => question = Some(arg.trim().to_owned()),
        }
    }
    if choices.len() > CHOICES {
        return Err(format!("at most {CHOICES} choices"));
    }
    Ok((question.ok_or(USAGE)?, choices))
}

/// `bot`'s open questions, oldest first.
pub fn open(bot: &str) -> Vec<Ask> {
    let Some(dir) = dir(bot) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !name.starts_with('.') && name.ends_with(".json"))
        })
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|file| {
            let read: Value = serde_json::from_slice(&std::fs::read(&file).ok()?).ok()?;
            let question = read["question"].as_str()?.to_owned();
            let choices = read["choices"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|choice| choice.as_str().map(str::to_owned))
                .collect();
            Some(Ask {
                file,
                question,
                choices,
            })
        })
        .collect()
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

/// The question answered, gone from the panel.
pub fn done(ask: &Ask) {
    let _ = std::fs::remove_file(&ask.file);
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

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn a_question_takes_choices_and_nothing_else() {
        assert_eq!(
            parse(&args(&[
                "Keep the old API?",
                "--choice",
                "Keep it",
                "--choice",
                "Drop it"
            ])),
            Ok((
                "Keep the old API?".into(),
                vec!["Keep it".into(), "Drop it".into()]
            ))
        );
        for wrong in [
            &[][..],
            &["--choice", "A"],
            &["Q?", "--choice"],
            &["Q?", "Another?"],
            &["Q?", "--bot", "x"],
        ] {
            assert_eq!(parse(&args(wrong)), Err(USAGE.to_owned()), "{wrong:?}");
        }
    }

    #[test]
    fn a_number_answers_with_its_choice() {
        let ask = Ask {
            file: PathBuf::new(),
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
            file: PathBuf::new(),
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
