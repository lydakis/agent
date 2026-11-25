//! Away: when the person has not used Sonny for a while, every lead is told
//! so, and each is told they are back once they look at it again, so it can
//! keep what does not need them and tell it in one message on their return,
//! as a colleague holds what came in while someone was out.
//!
//! The person is here when they use Sonny: a key, a click, a paste or focus
//! in any window ([`crate::used::Marks`]), or a message through `sonny acp`,
//! as from a chat bridge. Each use touches one file,
//! `~/.agent/sonny/present`. With no window open and no message, nothing
//! touches it, so they are away.
//!
//! Sonny's scheduler, which runs whether or not a window is open, looks at
//! that file once a minute ([`due`]). Once `[sonny] away_minutes` (20 by
//! default, 0 never) have passed since it was touched, it runs `sonny away`,
//! which attaches a note to every lead, starting no turn: the person is
//! away since then. It keeps which leads it tells, and since when, in
//! `~/.agent/sonny/away`, written before the first note: a lead the daemon
//! did not answer for is told again the next minute, under the same request
//! ID, until every lead answered.
//!
//! The person is back for a lead when they look at it: use its window, open
//! it, switch to it, or write to it through `sonny acp` ([`here`]). Only that
//! lead hears so then: as a steer, when it heard anything while they were
//! away, which answers with what it kept, else attached. Every other lead
//! stays away until they look at it, however long, and its back note covers
//! all it kept until then; one still away when they go again is not told
//! again, its absence goes on. The back note names the absence's start in
//! its request ID, so one sent twice, as by two windows at once, is one.
//!
//! Both notes say what happened and nothing about what to do: the lead's
//! role says that. Leads of the daemon the scheduler reaches by default
//! (`AGENT_SOCKET`, else that of `AGENT_STORE` or `~/.agent/state.sqlite`)
//! are told, and that daemon, kept in the file, hears they are back.
use agent_client::Client;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
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
    /// When the person last used Sonny before the latest absence the leads
    /// were told of, in ms.
    since_ms: i64,
    /// Each lead told the person is away, or being told, and not yet that
    /// they are back.
    leads: BTreeMap<String, Lead>,
    /// The socket of the daemon they are on, which hears they are back too.
    socket: PathBuf,
}

/// A lead's absence: since when the person is away from it, and whether
/// the daemon answered for its away note.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Lead {
    since_ms: i64,
    told: bool,
}

fn read_away() -> Option<Away> {
    let text = std::fs::read_to_string(away_file()?).ok()?;
    let kept: Value = serde_json::from_str(&text).ok()?;
    let mut leads = BTreeMap::new();
    for (name, lead) in kept["leads"].as_object()? {
        let lead = Lead {
            since_ms: lead["since_ms"].as_i64()?,
            told: lead["told"].as_bool()?,
        };
        leads.insert(name.clone(), lead);
    }
    Some(Away {
        since_ms: kept["since_ms"].as_i64()?,
        socket: PathBuf::from(kept["socket"].as_str()?),
        leads,
    })
}

fn write_away(away: &Away) -> Result<(), String> {
    let file = away_file().ok_or("no HOME")?;
    let leads: Map<String, Value> = (away.leads.iter())
        .map(|(name, lead)| {
            let lead = json!({"since_ms": lead.since_ms, "told": lead.told});
            (name.clone(), lead)
        })
        .collect();
    let text =
        json!({"since_ms": away.since_ms, "leads": leads, "socket": away.socket}).to_string();
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

/// Whether some leads are yet to answer for the away note of the absence
/// the person is still on.
fn is_unfinished(present: Option<i64>, away: Option<&Away>) -> bool {
    away.is_some_and(|away| {
        present == Some(away.since_ms)
            && (away.leads.values()).any(|lead| lead.since_ms == away.since_ms && !lead.told)
    })
}

/// The leads `sonny away` tells of the absence from `since_ms`, of those
/// the daemon lists (`names`), and what the file keeps then: a lead the
/// person has not looked at since an earlier absence stays away from that
/// one, untold again; every other lead is told of this one. A lead the
/// daemon no longer lists is gone from it.
fn to_tell(
    kept: Option<&Away>,
    names: &[String],
    since_ms: i64,
) -> (Vec<String>, BTreeMap<String, Lead>) {
    let mut tell = Vec::new();
    let mut leads = BTreeMap::new();
    for name in names {
        match kept.and_then(|kept| kept.leads.get(name)) {
            Some(lead) if lead.told => {
                leads.insert(name.clone(), *lead);
            }
            _ => {
                let lead = Lead {
                    since_ms,
                    told: false,
                };
                leads.insert(name.clone(), lead);
                tell.push(name.clone());
            }
        }
    }
    (tell, leads)
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
    is_due(present, told, minutes(), ms(SystemTime::now())) || is_unfinished(present, away.as_ref())
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
/// person is away, and keep whom it tells; a lead the daemon did not
/// answer for is told again next minute, while they are still away.
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
    let since_ms = present_ms().ok_or("the person never used Sonny")?;
    let socket = socket()?;
    let client = connect(&socket).await?;
    let names: Vec<String> = (every_lead(&client).await?.iter())
        .filter_map(|lead| lead["name"].as_str().map(str::to_owned))
        .collect();
    let kept = read_away().filter(|kept| kept.socket == socket);
    let (tell, leads) = to_tell(kept.as_ref(), &names, since_ms);
    // Kept before the first note, with every lead in it: one the daemon
    // took but did not answer for is told again next minute, and hears
    // the person is back once they look at it.
    write_away(&Away {
        since_ms,
        leads,
        socket: socket.clone(),
    })?;
    let note = away_note(since_ms);
    let id = format!("sonny-away-{since_ms}");
    let (mut told, mut failed) = (Vec::new(), None);
    for name in tell {
        match send(&client, &name, &note, &id, "attach").await {
            Ok(()) => told.push(name),
            Err(error) => failed = Some(format!("{name}: {error}")),
        }
    }
    // Read again: a lead looked at meanwhile is gone from it.
    let Some(mut away) = read_away().filter(|now| now.since_ms == since_ms) else {
        return Ok(());
    };
    for name in told {
        if let Some(lead) = away.leads.get_mut(&name)
            && lead.since_ms == since_ms
        {
            lead.told = true;
        }
    }
    // Back while they were being told: a lead not told they went has
    // nothing to hear of it.
    if present_ms().is_some_and(|present| present > since_ms) {
        away.leads
            .retain(|_, lead| lead.since_ms != since_ms || lead.told);
    }
    write_away(&away)?;
    match failed {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Whether `lead` heard anything since `since_ms`: a turn runs, or its
/// last one began or ended since.
fn heard_since(lead: &Value, since_ms: i64) -> bool {
    !lead["running_turn"].is_null()
        || ["started_ms", "finished_ms"]
            .iter()
            .any(|at| lead["last"][at].as_i64().is_some_and(|at| at > since_ms))
}

/// Tell `bot`, a lead told the person went at `since_ms`, that they are
/// back: a steer, when it heard anything since, which answers with what
/// it kept, else attached, so it knows too. It is gone from the file then,
/// and the file once no lead is left in it; one the daemon refuses is told
/// with the next look.
async fn tell_back(client: &Client, bot: &str, since_ms: i64) -> Result<(), String> {
    let listed = client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await
        .map_err(|e| e.to_string())?;
    let lead = (listed["bots"].as_array().into_iter().flatten())
        .find(|lead| lead["name"].as_str() == Some(bot));
    // Gone: no one to tell.
    if let Some(lead) = lead {
        let delivery = match heard_since(lead, since_ms) {
            true => "steer",
            false => "attach",
        };
        let id = format!("sonny-back-{since_ms}");
        send(client, bot, &back_note(since_ms), &id, delivery).await?;
    }
    let Some(mut away) = read_away() else {
        return Ok(());
    };
    if away
        .leads
        .get(bot)
        .is_none_or(|lead| lead.since_ms != since_ms)
    {
        return Ok(());
    }
    away.leads.remove(bot);
    match away.leads.is_empty() {
        true => {
            if let Some(file) = away_file() {
                let _ = std::fs::remove_file(file);
            }
            Ok(())
        }
        false => write_away(&away),
    }
}

/// The person used Sonny just now, looking at `bot` when there is one:
/// touch the presence file, and when that agent is a lead told they are
/// away, tell it they are back, off the caller's thread. Every other lead
/// told so stays away until they look at it. A window calls this at most
/// once every [`crate::used::EVERY`].
pub fn here(bot: Option<&str>) {
    // A test's window tells no lead of the person running it.
    if cfg!(test) {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let bot = bot.map(str::to_owned);
    runtime.spawn(async move {
        let Ok(Some((socket, bot, since_ms))) =
            tokio::task::spawn_blocking(move || touch(bot)).await
        else {
            return;
        };
        if let Ok(client) = connect(&socket).await {
            let _ = tell_back(&client, &bot, since_ms).await;
        }
    });
}

/// Touch the presence file; then `bot`, when it is a lead the daemon
/// answered for its away note, with its daemon's socket and when the
/// person went. One still being told hears they are back with the next
/// look, after its away note.
fn touch(bot: Option<String>) -> Option<(PathBuf, String, i64)> {
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
    let bot = bot?;
    let away = read_away()?;
    let since_ms = away.leads.get(&bot).filter(|lead| lead.told)?.since_ms;
    Some((away.socket, bot, since_ms))
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
    fn leads_yet_to_answer_for_the_away_note_are_told_again_until_all_did() {
        let away = |told| Away {
            since_ms: AT,
            leads: BTreeMap::from([
                ("shop.lead".into(), Lead { since_ms: AT, told }),
                // Away since an earlier absence, never looked at since.
                (
                    "calc.lead".into(),
                    Lead {
                        since_ms: AT - 1,
                        told: true,
                    },
                ),
            ]),
            socket: PathBuf::from("/tmp/agent.sock"),
        };
        // Still away, some leads not answered for: again.
        assert!(is_unfinished(Some(AT), Some(&away(false))));
        // All answered for.
        assert!(!is_unfinished(Some(AT), Some(&away(true))));
        // Back: none is told it went.
        assert!(!is_unfinished(Some(AT + 1), Some(&away(false))));
        assert!(!is_unfinished(Some(AT), None));
    }

    #[test]
    fn a_lead_not_looked_at_since_an_earlier_absence_stays_away_from_it() {
        let names = ["calc.lead", "shop.lead", "web.lead"].map(String::from);
        // The first absence: every lead is told.
        let (tell, leads) = to_tell(None, &names, AT);
        assert_eq!(tell, names);
        assert!(leads.values().all(|lead| *lead
            == Lead {
                since_ms: AT,
                told: false
            }));
        // The person looked at shop.lead only, and went again: calc.lead
        // and web.lead are still away from the first, and not told again.
        let kept = Away {
            since_ms: AT,
            leads: BTreeMap::from([
                (
                    "calc.lead".into(),
                    Lead {
                        since_ms: AT,
                        told: true,
                    },
                ),
                (
                    "web.lead".into(),
                    Lead {
                        since_ms: AT,
                        told: true,
                    },
                ),
                // Never answered for: told of this absence.
                (
                    "gone.lead".into(),
                    Lead {
                        since_ms: AT,
                        told: true,
                    },
                ),
            ]),
            socket: PathBuf::from("/tmp/agent.sock"),
        };
        let later = AT + 3_600_000;
        let names = ["calc.lead", "shop.lead", "web.lead", "new.lead"].map(String::from);
        let (tell, leads) = to_tell(Some(&kept), &names, later);
        assert_eq!(tell, ["shop.lead", "new.lead"]);
        assert_eq!(
            leads["calc.lead"],
            Lead {
                since_ms: AT,
                told: true
            }
        );
        assert_eq!(
            leads["web.lead"],
            Lead {
                since_ms: AT,
                told: true
            }
        );
        assert_eq!(
            leads["shop.lead"],
            Lead {
                since_ms: later,
                told: false
            }
        );
        assert_eq!(
            leads["new.lead"],
            Lead {
                since_ms: later,
                told: false
            }
        );
        // A lead the daemon no longer lists is gone.
        assert!(!leads.contains_key("gone.lead"));
        // One not answered for before is told again.
        let mut unanswered = kept.clone();
        unanswered.leads.insert(
            "shop.lead".into(),
            Lead {
                since_ms: AT,
                told: false,
            },
        );
        let (tell, _) = to_tell(Some(&unanswered), &["shop.lead".to_owned()], AT);
        assert_eq!(tell, ["shop.lead"]);
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
