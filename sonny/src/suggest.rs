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
use agent_client::Client;
use serde_json::json;
use std::sync::Arc;
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
- New ideas they didn't ask about
- Multiple sentences
Say nothing if the next step isn't obvious from what they said.
Say nothing if a suggestion could be unsafe or inappropriate, including any sensitive topic (security incidents, credentials, harm, private data).
Format: 2-12 words, match the person's style. Or nothing.
Call no tool. Reply with ONLY the suggestion, no quotes or explanation.";

/// The guess of a copy of `bot` at its head, on `model` when the window's
/// turns run on one of their own; `None` when it says nothing, or nothing
/// worth showing.
pub async fn guess(client: Arc<Client>, bot: String, model: Option<String>) -> Option<String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let name = format!("{bot}.suggest-{:x}", stamp % 0x100_0000);
    let forked = client
        .request(
            "fork",
            json!({"source": bot, "bot": name, "allow": [],
                "expire_ms": EXPIRE.as_millis() as u64, "request_id": name}),
        )
        .await
        .ok()?;
    let reply = ask(&client, &name, &forked["bot_id"], model).await;
    let _ = client
        .request("delete", json!({"bot": name, "bot_id": forked["bot_id"]}))
        .await;
    clean(&reply?)
}

/// The copy's reply, interrupting a turn that runs past `WAIT`.
async fn ask(
    client: &Client,
    name: &str,
    id: &serde_json::Value,
    model: Option<String>,
) -> Option<String> {
    let submitted = client
        .request(
            "submit",
            json!({"bot": name, "bot_id": id, "request_id": format!("{name}.1"),
                "prompt": ASK, "model": model}),
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
