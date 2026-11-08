//! The person's likely next message, guessed after a turn they started
//! and shown dim in the empty composer for Tab to take, as Claude Code's
//! prompt suggestions and Codex's composer predictions are (Claude Code
//! 2.1.312, `[SUGGESTION MODE` and its filters; Codex's help article
//! "Composer predictions in Codex").
//!
//! A copy of the agent guesses: forked where the turn ended, with its
//! tools and model, so the request reads as the agent's next would and its
//! prompt cache is warm, but allowed no call. The daemon deletes it once
//! idle a minute, should Sonny not get to.
//!
//! A busy lead holds more than its last reply: what other agents told it,
//! the margin, the commons, replies of its own the person scrolled past.
//! A guess drawn from one of those says so, from a fixed set Sonny checks
//! (see [`Cue`]), so the person knows what they missed; one about the
//! reply they are reading says nothing more. Neither Claude Code nor
//! Codex names a source: their guesses only follow the last reply.
//!
//! When no next message is obvious, the slot would stay empty; with
//! `[sonny] suggest_ideas` on, the same call may fill it instead with an
//! idea for the work, apart from the thread, or a tip for one of Sonny's
//! own commands or keys that fits now, each labeled as what it is
//! (George's "engineer serendipity", FEEDBACK I7 and I4). A prediction
//! keeps its own rules and always comes first; this asks for no other
//! call, timer or tool. A tip names only what Sonny has ([`SHORTCUTS`]
//! and [`COMMANDS`], as `?` and the slash menu list them), so it stays
//! current, and Tab takes one that is a command, as it takes a guess.
use crate::{COMMANDS, SHORTCUTS};
use agent_client::Client;
use serde_json::json;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

/// How long the guess may take; a later one is not shown.
const WAIT: Duration = Duration::from_secs(60);
/// A copy left behind is deleted once idle this long.
const EXPIRE: Duration = Duration::from_secs(60);

/// Claude Code's own ask, in Sonny's words.
const ASK: &str = "[SUGGESTION MODE: Suggest what the person might naturally type next into Sonny.]
FIRST: Look at the person's recent messages and original request.
Your job is to predict what THEY would type - not what you think they should do.
THE TEST: Would they think \"I was just about to type that\"?
EXAMPLES:
They asked \"fix the bug and run tests\", bug is fixed → \"run the tests\"
After code written → \"try it out\"
You offered options → the one they would likely pick, based on the conversation
You asked to continue → \"yes\" or \"go ahead\"
Task complete, obvious follow-up → \"commit this\" or \"push it\"
After an error or misunderstanding → nothing (let them assess and correct)
Be specific: \"run the tests\" beats \"continue\".
NEVER SUGGEST:
- Evaluative (\"looks good\", \"thanks\")
- Questions (\"what about...?\")
- Your own voice (\"Let me...\", \"I'll...\", \"Here's...\")
- Ideas of your own they didn't ask about
- Multiple sentences
Say nothing if the next step isn't obvious from what they said.
Say nothing if a suggestion could be unsafe or inappropriate, including any sensitive topic (security incidents, credentials, harm, private data).
Format: 2-12 words, match the person's style. Or nothing.
If it comes from anything other than your last reply, add a second line with exactly one of: earlier (a reply of yours before the last), margin, commons, or the name of the agent who told you. About your last reply, no second line.
";

/// How the ask ends, whatever came before.
const REPLY: &str = "Call no tool. Reply with ONLY the suggestion, and that line when it applies, no quotes or explanation.";

/// What may stand where the ask would say nothing, each labeled, and
/// never in place of a guess.
const IDEAS: &str = "WHERE you would say nothing above, do not leave the slot empty: offer one of these two instead, with its label as the second line. Never in place of a suggestion.
- An idea, first choice: something worth doing for the person's work that neither of you has raised, apart from what this conversation is on, as they would ask for it. Draw on what you know of their work: the project, its code, what is left undone. Second line: idea
- A tip, when no idea would be worth their time: one of Sonny's commands or keys below put to use on this conversation, so it reads as something they could do now, not a feature: a command as they would type it, with words after it where it takes them (\"/side why did the build slow down\"); a key with what it would find or do here (\"ctrl+f finds the cache discussion\"). Not one they have used here. Second line: tip
The same format and the same NEVER list apply, except that an idea is one of your own. Say nothing only where a suggestion could be unsafe.
";

/// Commands and keys too plain, or too drastic, to be a tip: what every
/// composer has, and what stops or leaves.
const NOT_TIPS: [&str; 16] = [
    "/quit",
    "/pause",
    "/resume",
    "enter",
    "tab",
    "esc",
    "/",
    "!",
    "shift+enter",
    "↑↓",
    "home",
    "end",
    "pgup",
    "pgdn",
    "ctrl+c",
    "ctrl+l",
];

/// The ask: with ideas, what may stand in an empty slot, the commands
/// and keys a tip names, written once, and the tips this window has
/// shown already, so it offers another.
fn asked(ideas: Option<&[String]>) -> std::borrow::Cow<'static, str> {
    static PLAIN: LazyLock<String> = LazyLock::new(|| format!("{ASK}{REPLY}"));
    static WITH_IDEAS: LazyLock<String> = LazyLock::new(|| {
        let mut ask = format!("{ASK}{IDEAS}Sonny's commands:\n");
        for (name, does) in COMMANDS.iter().filter(|(name, _)| !NOT_TIPS.contains(name)) {
            ask.push_str(&format!("{name}: {does}\n"));
        }
        ask.push_str("Sonny's keys:\n");
        let keys = (SHORTCUTS.iter())
            .filter(|(keys, _)| !keys.split_whitespace().any(|key| NOT_TIPS.contains(&key)));
        for (keys, does) in keys {
            ask.push_str(&format!("{keys}: {does}\n"));
        }
        ask
    });
    match ideas {
        None => PLAIN.as_str().into(),
        Some([]) => format!("{}{REPLY}", *WITH_IDEAS).into(),
        Some(tipped) => format!(
            "{}Tips shown to them already, so not again: {}\n{REPLY}",
            *WITH_IDEAS,
            tipped.join(", ")
        )
        .into(),
    }
}

/// A guess at the person's next message, and the words after the key:
/// where it came from when that is not the reply they are reading, or
/// that it is no guess but an idea or a tip.
#[derive(Clone, Debug, PartialEq)]
pub struct Guess {
    pub text: String,
    pub cue: Option<Cue>,
}

impl Guess {
    /// Whether Tab takes it: all but a tip for a key, which is only read.
    pub fn takes(&self) -> bool {
        self.cue != Some(Cue::Tip) || self.text.starts_with('/')
    }
}

/// What the line after a guess names: the only words shown, so one the
/// model made up shows none.
#[derive(Clone, Debug, PartialEq)]
pub enum Cue {
    /// A reply of the agent's own before the last.
    Earlier,
    Margin,
    Commons,
    /// An agent the daemon keeps, by its name.
    Agent(String),
    /// No guess: an idea for the work, apart from the thread.
    Idea,
    /// No guess: one of Sonny's commands or keys that fits now.
    Tip,
}

impl Cue {
    /// The words drawn after the key.
    pub fn name(&self) -> &str {
        match self {
            Cue::Earlier => "earlier",
            Cue::Margin => "margin",
            Cue::Commons => "commons",
            Cue::Agent(name) => name,
            Cue::Idea => "idea",
            Cue::Tip => "tip",
        }
    }

    /// Says what the words are, not where they came from: never left
    /// out for room.
    pub fn labels(&self) -> bool {
        matches!(self, Cue::Idea | Cue::Tip)
    }
}

/// The guess of a copy of `bot` at its head, on `model` when the window's
/// turns run on one of their own, and, with `ideas` (the tips the window
/// has shown), an idea or a tip where it would say nothing; `None` when
/// it says nothing, or nothing worth showing.
pub async fn guess(
    client: Arc<Client>,
    bot: String,
    model: Option<String>,
    ideas: Option<Vec<String>>,
) -> Option<Guess> {
    guessed(client, bot, model, ideas).await.1
}

/// The copy's reply as it came, and the guess read from it.
async fn guessed(
    client: Arc<Client>,
    bot: String,
    model: Option<String>,
    ideas: Option<Vec<String>>,
) -> (Option<String>, Option<Guess>) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let name = format!("{bot}.suggest-{:x}", stamp % 0x100_0000);
    let Ok(forked) = client
        .request(
            "fork",
            json!({"source": bot, "bot": name, "allow": [],
                "expire_ms": EXPIRE.as_millis() as u64, "request_id": name}),
        )
        .await
    else {
        return (None, None);
    };
    let ideas = ideas.as_deref();
    let reply = ask(&client, &name, &forked["bot_id"], model, &asked(ideas)).await;
    let _ = client
        .request("delete", json!({"bot": name, "bot_id": forked["bot_id"]}))
        .await;
    let Some(reply) = reply else {
        return (None, None);
    };
    let guess = read(&client, &bot, &reply, ideas).await;
    (Some(reply), guess)
}

/// The guess in the copy's reply, its cue checked.
async fn read(client: &Client, bot: &str, reply: &str, ideas: Option<&[String]>) -> Option<Guess> {
    let (text, source) = split(reply)?;
    let text = clean(text)?;
    let cue = match source.map(|source| named(source, bot)) {
        Some(Named::Is(cue)) => Some(cue),
        Some(Named::Agent(names)) => kept(client, names).await.map(Cue::Agent),
        Some(Named::Not) | None => None,
    };
    shown(Guess { text, cue }, ideas)
}

/// The guess, unless it is an idea or a tip not asked for, or a tip for
/// something Sonny does not have, does not tip, or tipped already.
fn shown(guess: Guess, ideas: Option<&[String]>) -> Option<Guess> {
    match (&guess.cue, ideas) {
        (Some(cue), None) if cue.labels() => None,
        (Some(Cue::Tip), Some(tipped)) => {
            let first = tipped_by(&guess.text)?;
            (sonny_has(&first) && !tipped.contains(&first)).then_some(guess)
        }
        _ => Some(guess),
    }
}

/// The command or key a tip begins with, as Sonny names it.
pub fn tipped_by(tip: &str) -> Option<String> {
    tip.split_whitespace().next().map(str::to_lowercase)
}

/// Whether `first` is one of Sonny's commands or keys that may be a tip.
fn sonny_has(first: &str) -> bool {
    !NOT_TIPS.contains(&first)
        && match first.starts_with('/') {
            true => COMMANDS.iter().any(|(name, _)| *name == first),
            false => {
                (SHORTCUTS.iter()).any(|(keys, _)| keys.split_whitespace().any(|k| k == first))
            }
        }
}

/// The guess's line and, when there is one, the line after it naming its
/// source; `None` for more lines than that, which are not a guess.
fn split(reply: &str) -> Option<(&str, Option<&str>)> {
    let reply = reply.trim();
    let Some((text, source)) = reply.split_once('\n') else {
        return Some((reply, None));
    };
    let source = source.trim();
    (!source.contains('\n')).then_some((text, Some(source).filter(|s| !s.is_empty())))
}

/// What the line naming a guess's source names.
#[derive(Debug, PartialEq)]
enum Named {
    Is(Cue),
    /// An agent, by the names it may go by, to be found among the agents.
    Agent(Vec<String>),
    Not,
}

/// The source `said` names, to a copy of `bot`: one of the fixed words,
/// `bot` itself (its own earlier reply), or a name to look for, as given
/// and inside `bot`'s project.
fn named(said: &str, bot: &str) -> Named {
    let said = said
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '`' | '\'' | '.' | '*'))
        .trim();
    match said.to_lowercase().as_str() {
        "earlier" => return Named::Is(Cue::Earlier),
        "margin" | "the margin" => return Named::Is(Cue::Margin),
        "commons" | "the commons" => return Named::Is(Cue::Commons),
        "idea" | "an idea" => return Named::Is(Cue::Idea),
        "tip" | "a tip" => return Named::Is(Cue::Tip),
        _ => {}
    }
    // A name is one word of a name's letters: never a pattern.
    if said.is_empty()
        || !said
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Named::Not;
    }
    if said == bot {
        return Named::Is(Cue::Earlier);
    }
    let mut names = vec![said.to_owned()];
    if let Some(project) = crate::project::lead_project(bot) {
        let inside = format!("{project}.{said}");
        if inside == bot {
            return Named::Is(Cue::Earlier);
        }
        names.push(inside);
    }
    Named::Agent(names)
}

/// The first of `names` the daemon keeps an agent by.
async fn kept(client: &Client, names: Vec<String>) -> Option<String> {
    for name in names {
        let page = client
            .request("bots", json!({"name": name, "limit": 1}))
            .await
            .ok()?;
        if page["bots"][0]["name"].as_str() == Some(name.as_str()) {
            return Some(name);
        }
    }
    None
}

/// The copy's reply, interrupting a turn that runs past `WAIT`.
async fn ask(
    client: &Client,
    name: &str,
    id: &serde_json::Value,
    model: Option<String>,
    prompt: &str,
) -> Option<String> {
    let submitted = client
        .request(
            "submit",
            json!({"bot": name, "bot_id": id, "request_id": format!("{name}.1"),
                "prompt": prompt, "delivery": "steer", "model": model}),
        )
        .await
        .ok()?;
    let handle = submitted["handle"].as_str()?;
    let waited = client
        .request(
            "wait",
            json!({"handles": [handle], "timeout_ms": WAIT.as_millis() as u64}),
        )
        .await
        .ok()?;
    let outcome = &waited["results"][handle];
    if outcome["pending"] == true {
        if let Some(turn) = submitted["turn"].as_i64() {
            let _ = client
                .request("interrupt", json!({"bot": name, "turn": turn}))
                .await;
            let _ = client
                .request("wait", json!({"handles": [handle], "timeout_ms": 5_000}))
                .await;
        }
        return None;
    }
    (outcome["status"] == "completed" && outcome["text_truncated"] != true)
        .then(|| outcome["text"].as_str().map(str::to_owned))
        .flatten()
}

/// The reply as a message the person might send, or `None` for one that
/// is not: Claude Code's filters, as it reads its own model's guesses.
pub fn clean(reply: &str) -> Option<String> {
    let text = unwrap(reply.trim());
    let text = unlabel(text).trim();
    let text = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text)
        .trim();
    let lower = text.to_lowercase();
    let words = text.split_whitespace().count();
    let refused = text.is_empty()
        || lower == "done"
        // Words about the guess, not one.
        || matches!(lower.as_str(), "nothing" | "nothing." | "nothing found" | "nothing found.")
        || lower.starts_with("nothing to suggest")
        || lower.starts_with("no suggestion")
        || lower.contains("silence is")
        || ["stay silent", "stays silent", "staying silent"].iter().any(|s| lower.contains(s))
        || lower.trim_matches(|c: char| !c.is_alphanumeric()) == "silence"
        || wrapped(text)
        || ["api error:", "prompt is too long", "request timed out", "invalid api key"]
            .iter()
            .any(|s| lower.starts_with(s))
        || labelled(text)
        || (words < 2 && !text.starts_with('/') && !ONE_WORD.contains(&lower.as_str()))
        || words > 12
        || text.chars().count() >= 100
        || sentences(text)
        || text.contains(['\n', '*'])
        || EVALUATIVE.iter().any(|s| contains_word(&lower, s))
        || AGENT_VOICE.iter().any(|s| lower.starts_with(s));
    (!refused).then(|| text.to_owned())
}

/// Replies of one word a person sends.
const ONE_WORD: [&str; 17] = [
    "yes", "yeah", "yep", "yea", "yup", "sure", "ok", "okay", "push", "commit", "deploy", "stop",
    "continue", "check", "exit", "quit", "no",
];

const EVALUATIVE: [&str; 15] = [
    "thanks",
    "thank you",
    "looks good",
    "sounds good",
    "that works",
    "that worked",
    "that's all",
    "nice",
    "great",
    "perfect",
    "makes sense",
    "awesome",
    "excellent",
    "lgtm",
    "well done",
];

/// How the agent, not the person, begins.
const AGENT_VOICE: [&str; 18] = [
    "let me",
    "i'll",
    "i've",
    "i'm",
    "i can",
    "i would",
    "i think",
    "i notice",
    "here's",
    "here is",
    "here are",
    "that's",
    "this is",
    "this will",
    "you can",
    "you should",
    "you could",
    "certainly",
];

/// The text inside one tag that wraps it all, as `<suggestion>…</suggestion>`.
fn unwrap(text: &str) -> &str {
    for tag in ["suggestion", "response", "output", "answer", "result"] {
        let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
        if let Some(inner) = text
            .get(..open.len())
            .filter(|head| head.eq_ignore_ascii_case(&open))
            .and_then(|_| text.get(open.len()..text.len().checked_sub(close.len())?))
            .filter(|_| text[text.len() - close.len()..].eq_ignore_ascii_case(&close))
            .filter(|inner| !inner.to_lowercase().contains(&close))
        {
            return inner;
        }
    }
    text
}

/// The text after a label the model put before it, as `Suggestion:`.
fn unlabel(text: &str) -> &str {
    for label in [
        "suggested response",
        "suggested reply",
        "suggested input",
        "suggested prompt",
        "suggestion",
        "response",
        "reply",
        "answer",
        "output",
        "result",
    ] {
        // The labels are ASCII, so the label's bytes are the text's.
        if let Some(after) = text
            .get(..label.len())
            .filter(|head| head.eq_ignore_ascii_case(label))
            .and_then(|_| text[label.len()..].trim_start().strip_prefix(':'))
        {
            return after;
        }
    }
    text
}

/// Wholly in brackets, as a note about the guess is.
fn wrapped(text: &str) -> bool {
    [('(', ')'), ('[', ']'), ('（', '）'), ('【', '】')]
        .iter()
        .any(|(open, close)| text.starts_with(*open) && text.ends_with(*close))
}

/// Begun with one word and a colon, as `Note: …`.
fn labelled(text: &str) -> bool {
    text.split_once(':').is_some_and(|(head, rest)| {
        !head.is_empty()
            && head.chars().all(|c| c.is_alphanumeric() || c == '_')
            && rest.starts_with(char::is_whitespace)
    })
}

/// More than one sentence: an end, a space, and a capital.
fn sentences(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    chars
        .windows(3)
        .any(|w| matches!(w[0], '.' | '!' | '?') && w[1].is_whitespace() && w[2].is_uppercase())
}

/// `phrase` in `text`, starting and ending on a word's edge.
fn contains_word(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + phrase.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_message_in_the_person_s_words_is_kept() {
        assert_eq!(clean("run the tests").as_deref(), Some("run the tests"));
        assert_eq!(clean("  commit this\n").as_deref(), Some("commit this"));
        assert_eq!(clean("yes").as_deref(), Some("yes"));
        assert_eq!(clean("/review").as_deref(), Some("/review"));
    }

    #[test]
    fn what_wraps_or_labels_it_is_taken_off() {
        assert_eq!(
            clean("<suggestion>push it</suggestion>").as_deref(),
            Some("push it")
        );
        assert_eq!(clean("Suggestion: push it").as_deref(), Some("push it"));
        assert_eq!(clean("\"try it out\"").as_deref(), Some("try it out"));
        // A letter whose lowercase is longer, as İ's is.
        assert_eq!(clean("Suggestion: İleri git").as_deref(), Some("İleri git"));
        assert_eq!(clean("SUGGESTION: İİİ İİİ").as_deref(), Some("İİİ İİİ"));
    }

    #[test]
    fn saying_nothing_shows_nothing() {
        for said in [
            "",
            "Nothing.",
            "nothing to suggest here",
            "No suggestion",
            "(staying silent)",
            "[none]",
            "Silence",
            "done",
        ] {
            assert_eq!(clean(said), None, "{said}");
        }
    }

    #[test]
    fn what_the_person_would_not_type_is_refused() {
        for said in [
            "looks good",
            "Thanks, that worked",
            "Let me run the tests",
            "I'll commit this now",
            "You should run the tests",
            "Note: run the tests",
            "Run the tests. Then push.",
            "run **all** the tests",
            "run the tests\nand push",
            "continue with the remaining eleven steps of the plan as we discussed earlier",
            "tests",
            "API error: overloaded",
        ] {
            assert_eq!(clean(said), None, "{said}");
        }
        assert!(clean(&"word ".repeat(30)).is_none());
    }

    #[test]
    fn a_second_line_names_the_source_and_more_is_no_guess() {
        assert_eq!(split("run the tests"), Some(("run the tests", None)));
        assert_eq!(
            split("check pricing's failing test\nshop.pricing\n"),
            Some(("check pricing's failing test", Some("shop.pricing")))
        );
        assert_eq!(split("push it\n  \n"), Some(("push it", None)));
        assert_eq!(split("push it\nshop.pricing\nbecause it failed"), None);
    }

    #[test]
    fn only_the_fixed_set_or_an_agent_s_name_is_a_source() {
        let bot = "shop.lead";
        assert_eq!(named("earlier", bot), Named::Is(Cue::Earlier));
        assert_eq!(named("Margin", bot), Named::Is(Cue::Margin));
        assert_eq!(named("the commons.", bot), Named::Is(Cue::Commons));
        // Its own name is its own earlier reply.
        assert_eq!(named("shop.lead", bot), Named::Is(Cue::Earlier));
        assert_eq!(named("lead", bot), Named::Is(Cue::Earlier));
        assert_eq!(
            named("pricing", bot),
            Named::Agent(vec!["pricing".into(), "shop.pricing".into()])
        );
        assert_eq!(
            named("`web.lead`", bot),
            Named::Agent(vec!["web.lead".into(), "shop.web.lead".into()])
        );
        // Words, or a pattern, name no agent.
        assert_eq!(named("your last reply", bot), Named::Not);
        assert_eq!(named("shop.[a-z]*", bot), Named::Not);
    }

    #[test]
    fn a_prediction_keeps_its_own_rules_and_ideas_only_follow_them() {
        // With ideas off, the ask is Claude Code's, as it was.
        assert!(asked(None).starts_with("[SUGGESTION MODE"));
        assert!(asked(None).ends_with(REPLY));
        assert!(!asked(None).contains("idea"));
        // With them on, every rule of a prediction comes first, unchanged,
        // and what may fill an empty slot only after.
        let with = asked(Some(&[]));
        assert!(with.starts_with(ASK));
        assert!(with[ASK.len()..].starts_with("WHERE you would say nothing above"));
        assert!(with.ends_with(REPLY));
        // Each command and key a tip may name, as the menu and `?` list
        // them, and nothing else written of Sonny.
        assert!(with.contains("/side: Start a side chat with a copy of this agent\n"));
        assert!(with.contains("alt+t ctrl+x: boost the next message; again for more\n"));
        // Nothing every composer has, nor what stops or leaves.
        assert!(!with.contains("/quit:") && !with.contains("ctrl+c ") && !with.contains("\nesc:"));
        // The tips shown already, so another comes.
        let after = asked(Some(&["ctrl+r".into(), "/side".into()]));
        assert!(after.starts_with(with.strip_suffix(REPLY).unwrap()));
        assert!(after.contains("Tips shown to them already, so not again: ctrl+r, /side\n"));
    }

    #[test]
    fn an_idea_or_a_tip_is_labeled_and_shown_only_when_asked_for() {
        let bot = "shop.lead";
        assert_eq!(named("idea", bot), Named::Is(Cue::Idea));
        assert_eq!(named("Tip.", bot), Named::Is(Cue::Tip));
        let idea = Guess {
            text: "add a cold start benchmark".into(),
            cue: Some(Cue::Idea),
        };
        assert_eq!(shown(idea.clone(), Some(&[])), Some(idea.clone()));
        assert_eq!(shown(idea, None), None);
        let tip = |text: &str| Guess {
            text: text.into(),
            cue: Some(Cue::Tip),
        };
        assert!(shown(tip("/zen the release"), Some(&[])).is_some());
        assert!(shown(tip("/zen the release"), None).is_none());
        assert!(shown(tip("Ctrl+R finds it again"), Some(&[])).is_some());
        assert!(shown(tip("alt+t boosts the next message"), Some(&[])).is_some());
        assert!(shown(tip("ctrl+x boosts it more"), Some(&[])).is_some());
        // Only what Sonny has.
        assert!(shown(tip("/teleport home"), Some(&[])).is_none());
        assert!(shown(tip("/zenith the release"), Some(&[])).is_none());
        assert!(shown(tip("ctrl+z undoes it"), Some(&[])).is_none());
        // Nor what is no tip, nor one shown already.
        assert!(shown(tip("/quit for tonight"), Some(&[])).is_none());
        assert!(shown(tip("ctrl+c copies it"), Some(&[])).is_none());
        let tipped = ["ctrl+r".to_owned()];
        assert!(shown(tip("Ctrl+R finds it again"), Some(&tipped)).is_none());
        assert!(shown(tip("/zen the release"), Some(&tipped)).is_some());
        // Tab takes a command, never a key's tip, which is only read.
        assert!(tip("/zen the release").takes());
        assert!(!tip("ctrl+r finds it again").takes());
        // A guess with no label is a guess, whatever the switch.
        let plain = Guess {
            text: "run the tests".into(),
            cue: None,
        };
        assert_eq!(shown(plain.clone(), None), Some(plain.clone()));
        assert!(plain.takes());
    }

    /// Not a unit test: the guesses a live daemon's agents make, each with
    /// ideas off and on, for `bench/suggest_eval.py`, which makes the
    /// agents and scores what this prints. Runs only when asked by it.
    #[tokio::test]
    #[ignore]
    async fn live() {
        let Ok(asked) = std::env::var("SONNY_SUGGEST_EVAL") else {
            return;
        };
        let asked: serde_json::Value = serde_json::from_str(&asked).unwrap();
        let (_, socket) = crate::daemon_paths(None, None).unwrap();
        let (client, _events) = Client::connect(&socket).await.unwrap();
        let rounds = asked["rounds"].as_u64().unwrap_or(1);
        for bot in asked["bots"].as_array().unwrap() {
            let bot = bot.as_str().unwrap();
            for round in 0..rounds {
                // One at a time: a copy is named by the millisecond.
                for ideas in [None, Some(Vec::new())] {
                    let on = ideas.is_some();
                    let (reply, guess) = guessed(client.clone(), bot.to_owned(), None, ideas).await;
                    let said = json!({"bot": bot, "ideas": on, "round": round, "reply": reply,
                        "text": guess.as_ref().map(|g| g.text.clone()),
                        "cue": guess.as_ref().and_then(|g| g.cue.as_ref()).map(Cue::name)});
                    println!("SUGGEST_EVAL {said}");
                }
            }
        }
    }

    #[test]
    fn a_word_inside_another_is_not_judged() {
        // "nice" inside "niceties", "great" inside "greater".
        assert_eq!(
            clean("drop the niceties from the greeter").as_deref(),
            Some("drop the niceties from the greeter")
        );
        assert_eq!(
            clean("make the timeout greater").as_deref(),
            Some("make the timeout greater")
        );
    }
}
