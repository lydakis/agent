//! What an agent left waiting on the person in its reply rather than with
//! the ask tool: a question in its words, a decision it named, a step only
//! the person can take. Once a turn of an agent of a Sonny project ends
//! with words and no question open, the lookout asks a copy of the agent,
//! with no tools, whether that reply waits on the person, and for what
//! ([`check`]). The copy runs at once, so it reads the history while the
//! provider's cache of the turn just ended is warm. A yes is kept as a
//! mark, `STORE.waiting/BOT.json`, holding the turn it covers and what the
//! person is waited on for ([`Mark`]); a no takes the mark away.
//!
//! Every surface that counts what waits on the person reads the mark as
//! that agent's open question while no later turn of its started
//! ([`crate::needs::Seen::open`]): the footer, `/status`, the agent's tab
//! and its lead's window, where a word typed under it goes to the agent.
//! `[sonny] needs_check = false` turns the check off; marks kept then
//! still hold until the agent's next turn.
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client::Client;
use serde_json::{Value, json};

/// How long the copy may take to answer.
pub const WAIT: Duration = Duration::from_secs(60);
/// How long the copy is kept, should the lookout stop before it is
/// deleted.
const EXPIRE: Duration = Duration::from_secs(180);
/// The most of what the person is waited on for a mark keeps, in
/// characters.
const FOR_CHARS: usize = 200;
/// What the copy is asked.
const ASK: &str = "[A CHECK FROM SONNY, answered in JSON alone; nothing here reaches the person.] \
    Read your last reply as the person will. Does it leave something only they can give before \
    your work goes on: an answer, a decision, an approval, or a step for them to take? With one, \
    set waiting to true and say in `for` what they need to give, in a few plain words of theirs, \
    such as \"which branch to deploy\". When your work goes on without them, or is finished, set \
    waiting to false and `for` to \"\".";

/// What a turn's reply left waiting on the person.
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    /// The turn whose reply it is: a later turn of the agent's lets it go.
    pub turn: i64,
    /// What the person is waited on for, in a few words.
    pub waiting_for: String,
}

impl Mark {
    /// The mark as the open question every surface shows: one with no
    /// options, so words typed under it go to the agent.
    pub fn ask(&self) -> crate::asks::Ask {
        crate::asks::Ask {
            id: crate::asks::id(self.turn, "waiting"),
            question: self.waiting_for.clone(),
            why: String::new(),
            options: Vec::new(),
            command: None,
            recommended: None,
            reason: String::new(),
            unanswered: None,
            deadline: None,
            covers: None,
            task: None,
        }
    }
}

/// Where the marks are kept: beside the store, one file per agent.
pub fn dir(store: &Path) -> PathBuf {
    let mut dir = store.as_os_str().to_owned();
    dir.push(".waiting");
    PathBuf::from(dir)
}

fn file(store: &Path, bot: &str) -> PathBuf {
    dir(store).join(format!("{bot}.json"))
}

/// The mark kept for `bot`, if any.
pub fn read(store: &Path, bot: &str) -> Option<Mark> {
    read_file(&file(store, bot))
}

/// The mark kept in `file`, if any.
pub fn read_file(file: &Path) -> Option<Mark> {
    let kept: Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    Some(Mark {
        turn: kept["turn"].as_i64()?,
        waiting_for: kept["for"].as_str()?.to_owned(),
    })
}

/// Keep `mark` for `bot`, or with none take its mark away, unless a mark
/// for a later turn is there already.
pub fn keep(store: &Path, bot: &str, turn: i64, mark: Option<&Mark>) -> Result<(), String> {
    if read(store, bot).is_some_and(|kept| kept.turn > turn) {
        return Ok(());
    }
    let path = file(store, bot);
    let failed = |e: std::io::Error| format!("{}: {e}", path.display());
    let Some(mark) = mark else {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(failed(e)),
            _ => Ok(()),
        };
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }
    // Its own, so two writers never write one file together.
    let next = path.with_extension(format!("json.{}.next", std::process::id()));
    let kept = json!({"turn": mark.turn, "for": mark.waiting_for});
    std::fs::write(&next, kept.to_string()).map_err(failed)?;
    std::fs::rename(&next, &path).map_err(failed)
}

/// Whether `mark` still says what its agent waits for: its turn is the
/// agent's newest, `newest`, and none runs.
pub fn holds(mark: &Mark, newest: Option<i64>, running: bool) -> bool {
    !running && newest == Some(mark.turn)
}

/// The JSON Schema the copy's reply follows.
fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "waiting": {"type": "boolean"},
            "for": {"type": "string"}
        },
        "required": ["waiting", "for"],
        "additionalProperties": false
    })
}

/// What the copy's reply says the person is waited on for: none when it
/// says nothing waits, or the reply is not the JSON asked for.
pub fn answer(reply: &str) -> Option<Option<String>> {
    let said: Value = serde_json::from_str(reply.trim()).ok()?;
    let waiting = said["waiting"].as_bool()?;
    let what = said["for"]
        .as_str()?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !waiting {
        return Some(None);
    }
    let what: String = what.chars().take(FOR_CHARS).collect();
    Some(Some(match what.is_empty() {
        true => "an answer to its last reply".to_owned(),
        false => what,
    }))
}

/// Ask a copy of `bot`, with no tools, whether its last reply waits on the
/// person: what for, or none; an error when the copy could not be made or
/// did not answer as asked.
pub async fn check(client: &Client, bot: &str) -> Result<Option<String>, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let name = copy_name(bot, stamp);
    let forked = client
        .request(
            "fork",
            json!({"source": bot, "bot": name, "allow": [],
                "expire_ms": EXPIRE.as_millis() as u64, "request_id": name}),
        )
        .await
        .map_err(|e| e.to_string())?;
    let reply = ask(client, &name, &forked["bot_id"]).await;
    let _ = client
        .request("delete", json!({"bot": name, "bot_id": forked["bot_id"]}))
        .await;
    let reply = reply?;
    answer(&reply).ok_or_else(|| format!("the check's reply was not the JSON asked for: {reply}"))
}

/// The copy's name: `bot` cut short enough that the name, and the submit's
/// request id after it, stay within the daemon's 128 bytes.
fn copy_name(bot: &str, stamp: u128) -> String {
    let tail = format!(".waiting-{:06x}", stamp % 0x100_0000);
    // Room for the ".1" the submit's request id adds.
    let mut keep = (126 - tail.len()).min(bot.len());
    while !bot.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{}{tail}", &bot[..keep])
}

/// Whether `bot` is a copy [`check`] made.
pub fn is_copy(bot: &str) -> bool {
    bot.rsplit_once(".waiting-")
        .is_some_and(|(_, tail)| tail.len() == 6 && tail.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The copy's reply, interrupting a turn that runs past [`WAIT`].
async fn ask(client: &Client, name: &str, id: &Value) -> Result<String, String> {
    let submitted = client
        .request(
            "submit",
            json!({"bot": name, "bot_id": id, "request_id": format!("{name}.1"),
                "prompt": ASK, "delivery": "reject", "output_schema": schema()}),
        )
        .await
        .map_err(|e| e.to_string())?;
    let handle = (submitted["handle"].as_str())
        .ok_or_else(|| format!("submit answered no handle: {submitted}"))?;
    let waited = client
        .request(
            "wait",
            json!({"handles": [handle], "timeout_ms": WAIT.as_millis() as u64}),
        )
        .await
        .map_err(|e| e.to_string())?;
    let outcome = &waited["results"][handle];
    if outcome["pending"] == true {
        if let Some(turn) = submitted["turn"].as_i64() {
            let _ = client
                .request("interrupt", json!({"bot": name, "turn": turn}))
                .await;
        }
        return Err(format!("still running after {}s", WAIT.as_secs()));
    }
    match outcome["status"].as_str() {
        Some("completed") => Ok(outcome["text"].as_str().unwrap_or("").to_owned()),
        status => Err(format!(
            "{} {}",
            status.unwrap_or("unknown"),
            outcome["error"]["code"].as_str().unwrap_or("")
        )
        .trim()
        .to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_copys_reply_says_what_the_person_is_waited_on_for() {
        assert_eq!(
            answer(r#"{"waiting": true, "for": "which  branch\nto deploy"}"#),
            Some(Some("which branch to deploy".into()))
        );
        assert_eq!(answer(r#"{"waiting": false, "for": ""}"#), Some(None));
        assert_eq!(
            answer(r#"{"waiting": true, "for": " "}"#),
            Some(Some("an answer to its last reply".into()))
        );
        assert_eq!(answer("Yes, it waits."), None);
        assert_eq!(answer(r#"{"for": "x"}"#), None);
    }

    #[test]
    fn a_mark_holds_until_a_later_turn_and_never_goes_back() {
        let store = std::env::temp_dir()
            .join(format!("sonny-waiting-{}", std::process::id()))
            .join("store");
        let mark = |turn: i64| Mark {
            turn,
            waiting_for: "which branch".into(),
        };
        keep(&store, "calc.a", 4, Some(&mark(4))).unwrap();
        assert_eq!(read(&store, "calc.a"), Some(mark(4)));
        // An older turn's answer, come late, changes nothing.
        keep(&store, "calc.a", 3, None).unwrap();
        assert_eq!(read(&store, "calc.a"), Some(mark(4)));
        let kept = read(&store, "calc.a").unwrap();
        assert!(holds(&kept, Some(4), false));
        assert!(!holds(&kept, Some(4), true));
        assert!(!holds(&kept, Some(5), false));
        // The next turn's check found nothing waiting.
        keep(&store, "calc.a", 5, None).unwrap();
        assert_eq!(read(&store, "calc.a"), None);
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }

    #[test]
    fn a_copys_name_fits_and_is_known() {
        let long = "p.".to_owned() + &"x".repeat(200);
        let name = copy_name(&long, 0xabcdef);
        assert!(name.len() <= 126);
        assert!(is_copy(&name));
        assert!(is_copy("calc.a.waiting-00ff10"));
        assert!(!is_copy("calc.waiting-room"));
    }
}
