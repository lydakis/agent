//! Away: when the person has not used Sonny for a while, every lead is told
//! so, and told again when they are back, so it can keep what does not need
//! them and tell it in one message on their return, as a colleague holds
//! what came in while someone was out.
//!
//! The person is here when they use Sonny: a key, a click, a paste or focus
//! in any window ([`crate::used::Marks`]), or a message through `sonny acp`,
//! as from a chat bridge. Each use touches one file,
//! `~/.agent/sonny/present`. With no window open and no message, nothing
//! touches it, so they are away.
//!
//! Sonny's scheduler, which runs whether or not a window is open, looks at
//! that file once a minute ([`due`]). Once `[sonny] away_minutes` (30 by
//! default, 0 never) have passed since it was touched, it runs `sonny away`,
//! which attaches a note to every lead, starting no turn: the person is
//! away since then. It keeps which leads it tells, and since when, in
//! `~/.agent/sonny/away`, written before the first note: a lead the daemon
//! did not answer for is told again the next minute, under the same request
//! ID, until every lead answered. The next use after that reads the file
//! ([`here`]) and tells those leads the person is back (a return while
//! `sonny away` still tells them, it tells of itself, after each lead's
//! away note): as a steer to each lead that heard
//! anything while they were away, which answers with what it kept, and
//! attached to the rest. Both notes name the absence's start in their
//! request IDs, so a note sent twice, as by two windows at once, is one.
//!
//! Both notes say what happened and nothing about what to do: the lead's
//! role says that. Leads of the daemon the scheduler reaches by default
//! (`AGENT_SOCKET`, else that of `AGENT_STORE` or `~/.agent/state.sqlite`)
//! are told, and that daemon, kept in the file, hears they are back.
use agent_client::Client;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// `sonny away`: what the scheduler runs once the person is away.
pub const VERB: &str = "away";
/// What the notes say they are from (`origin`).
pub const ORIGIN: &str = "sonny.away";

/// Sonny's own folder under HOME.
fn folder() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".agent/sonny"))
}

/// The file each use touches.
fn present_file() -> Option<PathBuf> {
    Some(folder()?.join("present"))
}

/// The file that says which leads were told the person is away, and since.
fn away_file() -> Option<PathBuf> {
    Some(folder()?.join("away"))
}

/// What `~/.agent/sonny/away` keeps.
#[derive(Debug, Clone, PartialEq)]
struct Away {
    /// When the person last used Sonny before they went, in ms.
    since_ms: i64,
    /// The leads told, or being told.
    leads: Vec<String>,
    /// Whether every one of them answered for the away note.
    told: bool,
    /// The socket of the daemon they are on, which hears they are back too.
    socket: PathBuf,
}

fn read_away() -> Option<Away> {
    let text = std::fs::read_to_string(away_file()?).ok()?;
    let kept: Value = serde_json::from_str(&text).ok()?;
    Some(Away {
        since_ms: kept["since_ms"].as_i64()?,
        told: kept["told"].as_bool()?,
        socket: PathBuf::from(kept["socket"].as_str()?),
        leads: (kept["leads"].as_array().into_iter().flatten())
            .filter_map(|lead| lead.as_str().map(str::to_owned))
            .collect(),
    })
}

fn write_away(away: &Away) -> Result<(), String> {
    let file = away_file().ok_or("no HOME")?;
    let text = json!({"since_ms": away.since_ms, "leads": away.leads, "told": away.told,
        "socket": away.socket})
    .to_string();
    let part = file.with_extension(format!("{}.part", std::process::id()));
    std::fs::write(&part, text).map_err(|e| e.to_string())?;
    std::fs::rename(&part, &file).map_err(|e| e.to_string())
}

fn ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

/// When the person last used Sonny, in ms; none before they ever did.
fn present_ms() -> Option<i64> {
    let modified = std::fs::metadata(present_file()?).ok()?.modified().ok()?;
    Some(ms(modified))
}

/// `[sonny] away_minutes`, read now: 0 never.
fn minutes() -> u64 {
    match crate::kept::config() {
        Ok(config) => config.sonny.away_minutes,
        Err(_) => agent_client::config::AWAY_MINUTES,
    }
}

/// Whether the person counts as away now: `minutes` (not 0) have passed
/// since their last use at `present`, and the leads were not told of this
/// absence yet (`told`, the start of the one they were told of).
fn is_due(present: Option<i64>, told: Option<i64>, minutes: u64, now_ms: i64) -> bool {
    let Some(present) = present else {
        return false;
    };
    minutes > 0
        && now_ms >= present + (minutes as i64) * 60_000
        && told.is_none_or(|since| since < present)
}

/// Whether the person is back from the absence the leads were told of,
/// and the leads are yet to hear so: told by the use itself, unless the
/// daemon could not be reached then.
fn is_back(present: Option<i64>, told: Option<i64>) -> bool {
    matches!((present, told), (Some(present), Some(since)) if present > since)
}

/// Whether some leads are yet to answer for the away note of the absence
/// the person is still on.
fn is_unfinished(present: Option<i64>, away: Option<&Away>) -> bool {
    away.is_some_and(|away| !away.told && present == Some(away.since_ms))
}

/// Whether `[sonny] away_minutes` is on, as config.toml says now: then
/// Sonny's scheduler runs, for this, with no automation.
pub fn on() -> bool {
    minutes() > 0
}

/// Whether the scheduler should run `sonny away` now: a stat of two small
/// files and a read of the config, once a minute.
pub fn due() -> bool {
    let (present, away) = (present_ms(), read_away());
    let told = away.as_ref().map(|away| away.since_ms);
    is_due(present, told, minutes(), ms(SystemTime::now()))
        || is_back(present, told)
        || is_unfinished(present, away.as_ref())
}

/// How the away note opens, and the back note.
const AWAY_OPENS: &str = "The person has not used Sonny since ";
const BACK_OPENS: &str = "The person is back.";

/// The away note: the absence's start, as the daemon's trailer names times.
fn away_note(since_ms: i64) -> String {
    format!(
        "{AWAY_OPENS}{}, so they are away. You will be told when they are back.",
        agent_client::clock::when(since_ms / 1000)
    )
}

/// The back note, naming when they went.
fn back_note(since_ms: i64) -> String {
    format!(
        "{BACK_OPENS} They were away from {}.",
        agent_client::clock::when(since_ms / 1000)
    )
}

/// The note's few words for the person's window: what it told the lead.
pub fn glance(text: &str) -> Option<&'static str> {
    if text.starts_with(AWAY_OPENS) {
        return Some("You are away");
    }
    text.starts_with(BACK_OPENS).then_some("You are back")
}

/// The socket of the daemon Sonny reaches by default (`AGENT_SOCKET`, else
/// that of `AGENT_STORE` or `~/.agent/state.sqlite`).
fn socket() -> Result<PathBuf, String> {
    Ok(match std::env::var_os("AGENT_SOCKET") {
        Some(socket) => PathBuf::from(socket),
        None => {
            let store = match std::env::var_os("AGENT_STORE") {
                Some(store) => PathBuf::from(store),
                None => folder()
                    .and_then(|sonny| Some(sonny.parent()?.join("state.sqlite")))
                    .ok_or("no HOME")?,
            };
            agent_client::socket::default_socket(&store).map_err(|e| e.to_string())?
        }
    })
}

/// The daemon at `socket`, if it runs: none is started for this.
async fn connect(socket: &std::path::Path) -> Result<std::sync::Arc<Client>, String> {
    let (client, _events) = Client::connect(socket).await.map_err(|e| e.to_string())?;
    Ok(client)
}

/// Every lead the daemon lists, page by page.
async fn every_lead(client: &Client) -> Result<Vec<Value>, String> {
    let mut leads = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let (page, next) = crate::project::every_lead(client, after.as_deref()).await?;
        leads.extend(page);
        match next {
            Some(next) => after = Some(next),
            None => return Ok(leads),
        }
    }
}

/// Send `prompt` to `bot` once for `request_id`: a note sent again, as by
/// another window at once, counts as sent.
async fn send(
    client: &Client,
    bot: &str,
    prompt: &str,
    request_id: &str,
    delivery: &str,
) -> Result<(), String> {
    let asked = json!({"bot": bot, "prompt": prompt, "request_id": request_id,
        "delivery": delivery, "origin": ORIGIN});
    match client.request("submit", asked).await {
        Ok(_) => Ok(()),
        Err(error) if error.code == "idempotency_conflict" => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// `sonny away`, run by the scheduler once [`due`]: tell every lead the
/// person is away, and keep whom it tells; or, back since and the leads not
/// told so yet, as when no daemon answered the use that brought them back,
/// tell them now. A use while the leads are told is a return, told at once.
pub fn cli() -> i32 {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return 1;
    };
    match runtime.block_on(tell_away()) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("sonny away: {error}");
            1
        }
    }
}

async fn tell_away() -> Result<(), String> {
    if !due() {
        return Ok(());
    }
    if let Some(away) = read_away()
        && is_back(present_ms(), Some(away.since_ms))
    {
        let client = connect(&away.socket).await?;
        return tell_back(&client, &away).await;
    }
    let since_ms = present_ms().ok_or("the person never used Sonny")?;
    let socket = socket()?;
    let client = connect(&socket).await?;
    let note = away_note(since_ms);
    let id = format!("sonny-away-{since_ms}");
    let leads = every_lead(&client).await?;
    let names = leads.iter().filter_map(|lead| lead["name"].as_str());
    // Kept before the first note, with every lead in it: one the daemon
    // took but did not answer for still hears the person is back, and one
    // not told yet is told again next minute.
    let mut away = Away {
        since_ms,
        leads: names.clone().map(str::to_owned).collect(),
        told: false,
        socket,
    };
    // Told before of this absence, in part: those leads stay in it.
    if let Some(before) = read_away().filter(|before| before.since_ms == since_ms) {
        for lead in before.leads {
            if !away.leads.contains(&lead) {
                away.leads.push(lead);
            }
        }
    }
    write_away(&away)?;
    let mut failed = None;
    for name in names {
        if let Err(error) = send(&client, name, &note, &id, "attach").await {
            failed = Some(format!("{name}: {error}"));
        }
    }
    // Back while the leads were being told: they hear so now.
    if present_ms().is_some_and(|present| present > since_ms) {
        return tell_back(&client, &away).await;
    }
    if let Some(error) = failed {
        return Err(error);
    }
    away.told = true;
    if read_away().is_some_and(|now| now.since_ms == since_ms) {
        write_away(&away)?;
    }
    Ok(())
}

/// Whether `lead` heard anything since `since_ms`: a turn runs, or its
/// last one began or ended since.
fn heard_since(lead: &Value, since_ms: i64) -> bool {
    !lead["running_turn"].is_null()
        || ["started_ms", "finished_ms"]
            .iter()
            .any(|at| lead["last"][at].as_i64().is_some_and(|at| at > since_ms))
}

/// Tell the leads told of `away` that the person is back: a steer to each
/// that heard anything since, which answers with what it kept, and
/// attached to the rest, which then know too. The file goes once all are
/// told; a lead the daemon refuses is told with the next use.
async fn tell_back(client: &Client, away: &Away) -> Result<(), String> {
    let note = back_note(away.since_ms);
    let id = format!("sonny-back-{}", away.since_ms);
    let mut failed = None;
    for lead in every_lead(client).await? {
        let Some(name) = lead["name"].as_str() else {
            continue;
        };
        if !away.leads.iter().any(|told| told == name) {
            continue;
        }
        let delivery = match heard_since(&lead, away.since_ms) {
            true => "steer",
            false => "attach",
        };
        if let Err(error) = send(client, name, &note, &id, delivery).await {
            failed = Some(error);
        }
    }
    if let Some(error) = failed {
        return Err(error);
    }
    if read_away().is_some_and(|now| now.since_ms == away.since_ms)
        && let Some(file) = away_file()
    {
        let _ = std::fs::remove_file(file);
    }
    Ok(())
}

/// The person used Sonny just now: touch the presence file, and when the
/// leads were told they are away, tell them they are back, off the caller's
/// thread. A window calls this at most once every [`crate::used::EVERY`].
pub fn here() {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    runtime.spawn(async {
        let Ok(Some(away)) = tokio::task::spawn_blocking(touch).await else {
            return;
        };
        if let Ok(client) = connect(&away.socket).await {
            let _ = tell_back(&client, &away).await;
        }
    });
}

/// Touch the presence file; what the leads were told, once every one of
/// them was. While `sonny away` still tells them, it tells them the person
/// is back too, after each lead's away note, as does its next run when it
/// stopped short.
fn touch() -> Option<Away> {
    let file = present_file()?;
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(opened) = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&file)
    {
        let _ = opened.set_modified(SystemTime::now());
    }
    read_away().filter(|away| away.told)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: i64 = 1_795_000_000_000;

    #[test]
    fn the_person_is_away_once_the_minutes_pass_and_told_once_for_each_absence() {
        let minute = 60_000;
        // Never used: never away.
        assert!(!is_due(None, None, 30, AT));
        // Not yet.
        assert!(!is_due(Some(AT), None, 30, AT + 29 * minute));
        assert!(is_due(Some(AT), None, 30, AT + 30 * minute));
        // Off.
        assert!(!is_due(Some(AT), None, 0, AT + 600 * minute));
        // Told of this absence already.
        assert!(!is_due(Some(AT), Some(AT), 30, AT + 90 * minute));
        // Told of an earlier one, and away again.
        assert!(is_due(
            Some(AT),
            Some(AT - 120 * minute),
            30,
            AT + 30 * minute
        ));
    }

    #[test]
    fn the_person_is_back_once_they_use_sonny_after_the_leads_were_told() {
        assert!(!is_back(Some(AT), None));
        assert!(!is_back(Some(AT), Some(AT)));
        assert!(is_back(Some(AT + 1), Some(AT)));
        assert!(!is_back(None, Some(AT)));
    }

    #[test]
    fn leads_yet_to_answer_for_the_away_note_are_told_again_until_all_did() {
        let away = |told| Away {
            since_ms: AT,
            leads: vec!["shop.lead".into()],
            told,
            socket: PathBuf::from("/tmp/agent.sock"),
        };
        // Still away, some leads not answered for: again.
        assert!(is_unfinished(Some(AT), Some(&away(false))));
        // All answered for.
        assert!(!is_unfinished(Some(AT), Some(&away(true))));
        // Back: the back note goes instead, to every lead in it.
        assert!(!is_unfinished(Some(AT + 1), Some(&away(false))));
        assert!(is_back(Some(AT + 1), Some(AT)));
        assert!(!is_unfinished(Some(AT), None));
    }

    #[test]
    fn a_lead_that_heard_anything_while_away_is_steered_the_rest_only_told() {
        let since = AT;
        assert!(heard_since(
            &json!({"running_turn": 7, "last": null}),
            since
        ));
        assert!(heard_since(
            &json!({"running_turn": null, "last": {"started_ms": since + 1}}),
            since
        ));
        // Began before they went, and ended while they were away.
        assert!(heard_since(
            &json!({"running_turn": null,
                "last": {"started_ms": since - 1, "finished_ms": since + 1}}),
            since
        ));
        assert!(!heard_since(
            &json!({"running_turn": null,
                "last": {"started_ms": since - 2, "finished_ms": since - 1}}),
            since
        ));
        assert!(!heard_since(
            &json!({"running_turn": null, "last": null}),
            since
        ));
    }

    #[test]
    fn the_notes_name_when_the_person_went_and_say_nothing_of_what_to_do() {
        let away = away_note(AT);
        let back = back_note(AT);
        assert_eq!(glance(&away), Some("You are away"));
        assert_eq!(glance(&back), Some("You are back"));
        assert_eq!(glance("The person wrote in calc.fix"), None);
        let when = agent_client::clock::when(AT / 1000);
        assert!(away.contains(&when), "{away}");
        assert!(back.contains(&when), "{back}");
        for note in [away, back] {
            for advice in ["tell", "should", "reply", "hold", "keep"] {
                assert!(!note.to_lowercase().contains(advice), "{note}");
            }
        }
    }
}
