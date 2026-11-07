//! What an agent left waiting on the person in its reply rather than with
//! the ask tool: a question in its words, a decision it named, a step only
//! the person can take. Once a turn of an agent of a Sonny project ends
//! with words and no question open, the window of its project that sees it
//! end asks a copy of the agent, with no tools, whether that reply waits on
//! the person, and for what ([`spawn`]). The copy runs at once, so it reads
//! the history while the provider's cache of the turn just ended is warm.
//! The answer is kept beside the store, `STORE.waiting/BOT.json`, holding
//! the turn it covers and, with a yes, what the person is waited on for
//! ([`Mark`]). It is the window's own work, as only a window shows it: a
//! turn that ends with no window of its project open is not checked, and
//! the daemon and its companions do nothing for it. Two windows that see
//! one turn end ask one copy: the copy is named after the agent and turn,
//! and the daemon refuses a second of that name while the first is asked,
//! and a turn answered already is not asked again.
//!
//! Every surface that counts what waits on the person reads a yes as that
//! agent's open question while no later turn of its started
//! ([`crate::needs::Seen::open`]): the footer, `/status`, the agent's tab
//! and its lead's window, where a word typed under it goes to the agent.
//! The same copy names the code reviews its turn opened, updated or waits
//! on, in whatever system holds them, as its commands' output or its reply
//! say ([`crate::reviews`]): a copy reads a homegrown tool as the agent
//! that used it did.
//! `[sonny] needs_check = false` turns the check off, and with it what
//! reviews are kept; marks kept then still hold until the agent's next
//! turn.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use agent_client::Client;
use serde_json::{Value, json};

/// How long the copy may take to answer.
pub const WAIT: Duration = Duration::from_secs(60);
/// How long the copy is kept, should its window close before it is
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
    waiting to false and `for` to \"\". In `reviews`, list each code review you opened, updated or wait \
    on in your last turn, in its commands' output or your reply, in whatever system holds it, by a \
    short name such as \"o/r#7\", \"D1234\" or \"CR-881\", else its link; [] for none.";

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

/// Whether `ask` is a mark's, as [`Mark::ask`] makes it, not a question
/// the agent asked.
pub fn is_mark(ask: &crate::asks::Ask) -> bool {
    ask.id.ends_with("/waiting")
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

/// Keep what `bot`'s turn `turn` was found to leave waiting: `mark`, or
/// with none the turn alone, which no surface reads as waiting, unless a
/// later turn's is there already.
pub fn keep(store: &Path, bot: &str, turn: i64, mark: Option<&Mark>) -> Result<(), String> {
    locked(&dir(store), || {
        if answered(store, bot, turn + 1) {
            return Ok(());
        }
        let kept = match mark {
            Some(mark) => json!({"turn": mark.turn, "for": mark.waiting_for}),
            None => json!({"turn": turn}),
        };
        replace(&file(store, bot), kept.to_string().as_bytes())
    })
}

/// Run `write`, a read of a file in `dir` and its replacement, while no
/// other window runs one there: a check's write never undoes a newer one
/// another window made between its read and its rename.
pub fn locked<T>(dir: &Path, write: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let failed = |e: std::io::Error| format!("{}: {e}", dir.display());
    std::fs::create_dir_all(dir).map_err(failed)?;
    let lock = (std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true))
    .open(dir.join(".lock"))
    .map_err(failed)?;
    lock.lock().map_err(failed)?;
    write()
}

/// Put `bytes` in place of `path` at once, written first beside it under
/// this process's own name.
pub fn replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("{}: {e}", path.display());
    let next = path.with_extension(format!("json.{}.next", std::process::id()));
    std::fs::write(&next, bytes).map_err(failed)?;
    std::fs::rename(&next, path).map_err(failed)
}

/// Whether what `bot`'s turn `turn`, or a later one, left waiting is kept.
fn answered(store: &Path, bot: &str, turn: i64) -> bool {
    std::fs::read(file(store, bot))
        .ok()
        .and_then(|kept| serde_json::from_slice::<Value>(&kept).ok())
        .and_then(|kept| kept["turn"].as_i64())
        .is_some_and(|kept| kept >= turn)
}

/// What a window last read of the folder of marks: when the folder last
/// changed, and when each agent's mark did.
#[derive(Default)]
pub struct Marks {
    changed: Option<SystemTime>,
    each: HashMap<String, Option<SystemTime>>,
}

impl Marks {
    /// The agents, of those `keep` takes, whose mark a check set, changed
    /// or took away since the folder was last read: none, after one stat,
    /// while the folder is as it was.
    pub fn moved(&mut self, store: &Path, keep: impl Fn(&str) -> bool) -> Vec<String> {
        let dir = dir(store);
        let changed = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
        if changed == self.changed {
            return Vec::new();
        }
        self.changed = changed;
        let mut now = HashMap::new();
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(bot) = name.strip_suffix(".json")
                && keep(bot)
            {
                let at = entry.metadata().and_then(|m| m.modified()).ok();
                now.insert(bot.to_owned(), at);
            }
        }
        let had = std::mem::replace(&mut self.each, now);
        let mut moved: Vec<String> = (self.each.iter())
            .filter(|(bot, at)| had.get(*bot) != Some(at))
            .map(|(bot, _)| bot.clone())
            .collect();
        moved.extend(had.into_keys().filter(|bot| !self.each.contains_key(bot)));
        moved
    }
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
            "for": {"type": "string"},
            "reviews": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["waiting", "for", "reviews"],
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

/// The code reviews the copy's reply says the reply named.
pub fn reviews(reply: &str) -> Vec<String> {
    let said: Value = serde_json::from_str(reply.trim()).unwrap_or_default();
    (said["reviews"].as_array().into_iter().flatten())
        .filter_map(|name| Some(name.as_str()?.to_owned()))
        .collect()
}

/// How many copies one window has asking at once.
const CHECKS: usize = 4;
static ASKING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(CHECKS);
/// `bot`'s record, as `bots` lists it.
async fn record(client: &Client, bot: &str) -> Option<Value> {
    let page = (client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await)
        .ok()?;
    Some(page["bots"][0].clone()).filter(|row| row["name"] == bot)
}

/// Whether `row`, the record of a bot named `bot`, is a side chat or a
/// check's copy: a task merely named `NAME.side-feed` or
/// `NAME.waiting-00ff10` is checked as any other.
fn side_or_copy(bot: &str, row: &Value) -> bool {
    // A check's copy expires and may call no tool.
    let copy = is_copy(bot) && row["expire_ms"].is_u64() && row["allowed"] == json!([]);
    crate::side_copy(row).is_some() || copy
}

/// The turns this window checks now, which it heard end more than once.
static CHECKING: std::sync::Mutex<Vec<(String, i64)>> = std::sync::Mutex::new(Vec::new());

/// How long after a turn ends a window that reads it ended checks it:
/// about as long as a provider keeps the turn cached. One older ended
/// while no window was there to check it.
const JUST_ENDED: Duration = Duration::from_secs(300);

/// Whether a turn that ended at `finished_ms` ended just now.
pub fn just_ended(finished_ms: u64) -> bool {
    (crate::home::now_ms() as u64).saturating_sub(finished_ms) < JUST_ENDED.as_millis() as u64
}

/// Check what `bot`'s turn `turn`, just ended, left waiting on the person,
/// on its own task so the window never waits on it: what a copy says is
/// kept as its mark, and the code reviews it says the reply named
/// ([`crate::reviews`]). Not with `[sonny] needs_check` off, nor a side
/// chat's, a copy's or a turn answered already.
pub fn spawn(client: Arc<Client>, store: PathBuf, bot: String, turn: i64) {
    if answered(&store, &bot, turn)
        || !agent_client::config::load().map_or(true, |c| c.sonny.needs_check)
    {
        return;
    }
    let key = (bot.clone(), turn);
    match CHECKING.lock() {
        Ok(mut checking) if !checking.contains(&key) => checking.push(key.clone()),
        _ => return,
    }
    tokio::spawn(async move {
        if let Some(row) = record(&client, &bot).await
            && let Some(bot_id) = row["bot_id"].as_i64()
            && !side_or_copy(&bot, &row)
            && let Ok(_asking) = ASKING.acquire().await
            // Another window answered it while this one waited its turn.
            && !answered(&store, &bot, turn)
            // What could not be checked, a second window's copy the
            // daemon refused among it, is left unmarked.
            && let Ok(found) = waiting_for(&client, &store, &bot, bot_id, turn).await
        {
            let mark = found.map(|waiting_for| Mark { turn, waiting_for });
            let _ = keep(&store, &bot, turn, mark.as_ref());
        }
        if let Ok(mut checking) = CHECKING.lock() {
            checking.retain(|kept| *kept != key);
        }
    });
}

/// What `bot`'s turn `turn` left waiting on the person, once the code
/// reviews its reply names are kept beside `store` ([`crate::reviews`]):
/// nothing for a turn that ended without words; else what a copy of `bot`
/// says, unless a question the ask tool put is open, which every surface
/// reads itself.
async fn waiting_for(
    client: &Client,
    store: &Path,
    bot: &str,
    bot_id: i64,
    turn: i64,
) -> Result<Option<String>, String> {
    let handle = format!("turn:{bot}/{turn}");
    let ended = client
        .request("wait", json!({"handles": [&handle], "timeout_ms": 0}))
        .await
        .map_err(|e| e.to_string())?;
    if ended["results"][&handle]["text"]
        .as_str()
        .is_none_or(|said| said.trim().is_empty())
    {
        return Ok(None);
    }
    let reply = self::check(client, bot, turn).await?;
    crate::reviews::note(store, bot, bot_id, turn, &reviews(&reply))?;
    let waiting = answer(&reply)
        .ok_or_else(|| format!("the check's reply was not the JSON asked for: {reply}"))?;
    let asked = crate::needs::scan(client, bot, Some(turn))
        .await
        .is_some_and(|seen| seen.ask.is_some());
    Ok(waiting.filter(|_| !asked))
}

/// Ask a copy of `bot`, with no tools, whether its turn `turn`'s reply
/// waits on the person, and what reviews it named: its reply, in JSON
/// that follows [`schema`]; an error when the copy could not be made, or
/// another window's copy of that turn is asked already.
pub async fn check(client: &Client, bot: &str, turn: i64) -> Result<String, String> {
    ask_copy(client, bot, turn, turn as u128 % 0x80_0000, ASK, schema()).await
}

/// What a copy of `bot`, with no tools, answers `prompt` in JSON that
/// follows `schema`: the copy, named by `stamp`, is made at the end of
/// `turn`, asked and deleted, and lives at most [`EXPIRE`] should its
/// window close first. While it is there the daemon refuses another
/// window's of that name.
pub async fn ask_copy(
    client: &Client,
    bot: &str,
    turn: i64,
    stamp: u128,
    prompt: &str,
    schema: Value,
) -> Result<String, String> {
    let checkpoint = turn_end(client, bot, turn).await?;
    // The copy answers on the model and effort the turn ran on, which may
    // be its own, not the bot's, and has that turn cached.
    let ran = client
        .request("turns", json!({"bot": bot, "after": turn - 1, "limit": 1}))
        .await
        .map_err(|e| e.to_string())?;
    let ran = Some(&ran["turns"][0]).filter(|ran| ran["turn"].as_i64() == Some(turn));
    let name = copy_name(bot, stamp);
    // This window's own request: another's, of the same name, is refused
    // while this copy is there.
    let request = format!("{name}.{}", std::process::id());
    let forked = client
        .request(
            "fork",
            json!({"source": bot, "bot": name, "allow": [], "checkpoint": checkpoint,
                "expire_ms": EXPIRE.as_millis() as u64, "request_id": request}),
        )
        .await
        .map_err(|e| e.to_string())?;
    let reply = ask(client, &name, &forked["bot_id"], ran, prompt, schema).await;
    let _ = client
        .request("delete", json!({"bot": name, "bot_id": forked["bot_id"]}))
        .await;
    reply
}

/// How far back a turn's last node is looked for among `bot`'s newest.
const TURN_END_NODES: usize = 64;

/// The last node of `bot`'s turn `turn`, where its copy forks, so it reads
/// that turn though another began since; an error when it is not among
/// the newest, too far behind to check.
async fn turn_end(client: &Client, bot: &str, turn: i64) -> Result<i64, String> {
    let page = client
        .request(
            "history_nodes",
            json!({"bot": bot, "limit": TURN_END_NODES}),
        )
        .await
        .map_err(|e| e.to_string())?;
    (page["nodes"].as_array().into_iter().flatten())
        .filter(|node| node["turn"].as_i64() == Some(turn))
        .filter_map(|node| node["node"].as_i64())
        .max()
        .ok_or_else(|| format!("turn {turn} of {bot} is not among its newest"))
}

/// The copy's name: `bot` cut short enough that the name, and the submit's
/// request id after it, stay within the daemon's 128 bytes.
fn copy_name(bot: &str, stamp: u128) -> String {
    let tail = format!(".waiting-{:06x}", stamp % 0x100_0000);
    // Room for the ".1" the submit's request id adds, or the fork's
    // process id.
    let mut keep = (116 - tail.len()).min(bot.len());
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
async fn ask(
    client: &Client,
    name: &str,
    id: &Value,
    ran: Option<&Value>,
    prompt: &str,
    schema: Value,
) -> Result<String, String> {
    let mut submit = json!({"bot": name, "bot_id": id, "request_id": format!("{name}.1"),
        "prompt": prompt, "delivery": "reject", "output_schema": schema});
    for field in ["model", "effort"] {
        if let Some(on) = ran.and_then(|ran| ran[field].as_str()) {
            submit[field] = json!(on);
        }
    }
    let submitted = client
        .request("submit", submit)
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
        // Kept as answered, so no other window asks it again.
        assert!(answered(&store, "calc.a", 5));
        assert!(!answered(&store, "calc.a", 6));
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

    /// A turn that ended with words and no question asked is put to a
    /// copy of its agent, with no tools and the check's schema; one that
    /// ended with none, or on the ask tool, is not.
    #[test]
    fn marks_moved_are_those_set_changed_or_taken_away_since() {
        let store = std::env::temp_dir()
            .join(format!("sonny-waiting-marks-{}", std::process::id()))
            .join("store");
        let _ = std::fs::remove_dir_all(dir(&store));
        let mut marks = Marks::default();
        assert!(marks.moved(&store, |_| true).is_empty());
        let mark = |w: &str| Mark {
            turn: 2,
            waiting_for: w.into(),
        };
        keep(&store, "calc.a", 2, Some(&mark("a"))).unwrap();
        keep(&store, "calc.b", 2, Some(&mark("b"))).unwrap();
        let mut moved = marks.moved(&store, |bot| bot != "calc.b");
        moved.sort();
        assert_eq!(moved, ["calc.a"]);
        // Nothing changed: one stat, nothing read.
        assert!(marks.moved(&store, |_| true).is_empty());
        std::fs::remove_file(file(&store, "calc.a")).unwrap();
        // calc.a's mark went; calc.b is taken now and new to it.
        let mut moved = marks.moved(&store, |_| true);
        moved.sort();
        assert_eq!(moved, ["calc.a", "calc.b"]);
        let _ = std::fs::remove_dir_all(dir(&store));
    }

    #[test]
    fn a_task_named_like_a_side_chat_or_copy_is_one_only_if_its_record_says() {
        let side = json!({"name": "shop.side-a1", "created_by": "shop", "expire_ms": 99});
        let feed = json!({"name": "shop.side-feed", "created_by": "shop.lead", "expire_ms": null});
        let copy = json!({"name": "shop.waiting-00ff11", "expire_ms": 99, "allowed": []});
        let task = json!({"name": "shop.waiting-00ff10", "created_by": "shop.lead",
            "expire_ms": null, "allowed": ["*"]});
        assert!(side_or_copy("shop.side-a1", &side));
        assert!(!side_or_copy("shop.side-feed", &feed));
        assert!(side_or_copy("shop.waiting-00ff11", &copy));
        assert!(!side_or_copy("shop.waiting-00ff10", &task));
        assert!(!side_or_copy("shop.web", &json!({"name": "shop.web"})));
    }

    #[tokio::test]
    async fn a_reply_with_words_is_checked_by_a_tool_less_copy() {
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let seen = asked.clone();
        let client = crate::tests::daemon_answering(move |request| {
            seen.lock().unwrap().push(request.clone());
            let handle = request["handles"][0].as_str().unwrap_or("");
            match request["op"].as_str() {
                Some("wait") if handle == "turn:calc.plan/3" => json!({"results": {handle:
                    {"status": "completed", "text":
                        "Sent CR-881 for review. Deploy to staging or prod?"}}}),
                Some("wait") if handle == "turn:calc.quiet/3" => json!({"results": {handle:
                    {"status": "completed", "text": " "}}}),
                Some("wait") => json!({"results": {handle: {"status": "completed",
                    "text": r#"{"waiting": true, "for": "staging or prod", "reviews": ["CR-881"]}"#}}}),
                // Turn 3 ended at node 12; a later turn began since.
                Some("history_nodes") => json!({"nodes": [{"node": 14, "turn": 4},
                    {"node": 12, "turn": 3}, {"node": 11, "turn": 3}]}),
                // The turn ran on a model of its own.
                Some("turns") => json!({"turns": [{"turn": 3, "model": "openai/gpt-x",
                    "effort": "high"}]}),
                Some("fork") => json!({"bot_id": 9}),
                Some("submit") => json!({"handle": "turn:copy/1", "turn": 1}),
                _ => json!({}),
            }
        })
        .await;
        let store = std::env::temp_dir()
            .join(format!("sonny-waiting-for-{}", std::process::id()))
            .join("store");
        assert_eq!(
            waiting_for(&client, &store, "calc.plan", 5, 3).await,
            Ok(Some("staging or prod".into()))
        );
        // The review the copy says its reply named, in any system, is
        // kept for `/status`.
        assert_eq!(crate::reviews::read(&store, "calc.plan", 5), ["CR-881"]);
        let requests = asked.lock().unwrap().clone();
        let fork = requests.iter().find(|r| r["op"] == "fork").unwrap();
        assert_eq!(fork["source"], "calc.plan");
        assert_eq!(fork["allow"], json!([]));
        // It reads the turn it checks, not the one begun since.
        assert_eq!(fork["checkpoint"], 12);
        let submit = requests.iter().find(|r| r["op"] == "submit").unwrap();
        assert_eq!(
            submit["output_schema"]["required"],
            json!(["waiting", "for", "reviews"])
        );
        // It answers on the model and effort the turn ran on.
        assert_eq!(
            (&submit["model"], &submit["effort"]),
            (&json!("openai/gpt-x"), &json!("high"))
        );
        assert!(requests.iter().any(|r| r["op"] == "delete"));
        // No words, no copy.
        asked.lock().unwrap().clear();
        assert_eq!(
            waiting_for(&client, &store, "calc.quiet", 6, 3).await,
            Ok(None)
        );
        assert!(!asked.lock().unwrap().iter().any(|r| r["op"] == "fork"));
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }
}
