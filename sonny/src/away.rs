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
//! ID, until every lead answered, or with its back note when the person is
//! back first. Every change to the file is made under a lock beside it, so
//! two windows' looks, or a look and the scheduler, never undo each other.
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

/// Change what `~/.agent/sonny/away` keeps, as one step among every
/// Sonny's: a window's look and the scheduler each read, change and write
/// it, and one's write never undoes another's. `change` gets what it keeps
/// and gives what it keeps next, none to have it go.
fn change<T>(change: impl FnOnce(Option<Away>) -> (Option<Away>, T)) -> Result<T, String> {
    let file = away_file().ok_or("no HOME")?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(file.with_extension("lock"))
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let (next, out) = change(read_away());
    match next {
        Some(away) => write_away(&away)?,
        None => match std::fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error.to_string());
            }
            _ => {}
        },
    }
    Ok(out)
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
    // Kept before the first note, with every lead in it: one the daemon
    // took but did not answer for is told again next minute, or, once the
    // person looks at it, again with its back note.
    let tell = change(|kept| {
        let kept = kept.filter(|kept| kept.socket == socket);
        let (tell, leads) = to_tell(kept.as_ref(), &names, since_ms);
        let away = Away {
            since_ms,
            leads,
            socket: socket.clone(),
        };
        (Some(away), tell)
    })?;
    let note = away_note(since_ms);
    let (mut told, mut failed) = (Vec::new(), None);
    for name in tell {
        match send(&client, &name, &note, &away_id(since_ms), "attach").await {
            Ok(()) => told.push(name),
            Err(error) => failed = Some(format!("{name}: {error}")),
        }
    }
    // A lead looked at meanwhile is gone from it, and stays gone.
    change(|now| {
        let now = now.map(|mut now| {
            for name in &told {
                if let Some(lead) = now.leads.get_mut(name)
                    && lead.since_ms == since_ms
                {
                    lead.told = true;
                }
            }
            now
        });
        (now, ())
    })?;
    match failed {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The request ID of the away note of the absence from `since_ms`.
fn away_id(since_ms: i64) -> String {
    format!("sonny-away-{since_ms}")
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
/// it kept, else attached, so it knows too. An away note the daemon did
/// not answer for (`told` false) goes again first, under its request ID:
/// a note it took counts as sent, one it did not goes now, before the back
/// note. The lead is gone from the file then, and the file once no lead is
/// left in it; one the daemon refuses is told with the next look.
async fn tell_back(client: &Client, bot: &str, lead: Lead) -> Result<(), String> {
    let since_ms = lead.since_ms;
    let listed = client
        .request("bots", json!({"name": bot, "limit": 1}))
        .await
        .map_err(|e| e.to_string())?;
    let listed = (listed["bots"].as_array().into_iter().flatten())
        .find(|lead| lead["name"].as_str() == Some(bot));
    // Gone: no one to tell.
    if let Some(listed) = listed {
        if !lead.told {
            send(
                client,
                bot,
                &away_note(since_ms),
                &away_id(since_ms),
                "attach",
            )
            .await?;
        }
        let delivery = match heard_since(listed, since_ms) {
            true => "steer",
            false => "attach",
        };
        let id = format!("sonny-back-{since_ms}");
        send(client, bot, &back_note(since_ms), &id, delivery).await?;
    }
    change(|away| {
        let away = away.and_then(|mut away| {
            if away
                .leads
                .get(bot)
                .is_some_and(|kept| kept.since_ms == since_ms)
            {
                away.leads.remove(bot);
            }
            (!away.leads.is_empty()).then_some(away)
        });
        (away, ())
    })
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
    runtime.spawn(back(bot.map(str::to_owned)));
}

/// What [`here`] does, to wait on: as a window that switches to `bot`
/// does before it goes, so the note is not cut short with it.
pub async fn back(bot: Option<String>) {
    if cfg!(test) {
        return;
    }
    let Ok(Some((socket, bot, lead))) = tokio::task::spawn_blocking(move || touch(bot)).await
    else {
        return;
    };
    if let Ok(client) = connect(&socket).await {
        let _ = tell_back(&client, &bot, lead).await;
    }
}

/// Touch the presence file; then `bot`, when it is a lead told the person
/// is away, with its daemon's socket and its absence.
fn touch(bot: Option<String>) -> Option<(PathBuf, String, Lead)> {
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
    let lead = *away.leads.get(&bot)?;
    Some((away.socket, bot, lead))
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
    fn looks_at_once_from_many_windows_each_take_their_own_lead_off() {
        // The one test of this file that writes it, under the tests' home.
        let names: Vec<String> = (0..16).map(|at| format!("p{at}.lead")).collect();
        let lead = Lead {
            since_ms: AT,
            told: true,
        };
        let away = Away {
            since_ms: AT,
            leads: names.iter().map(|name| (name.clone(), lead)).collect(),
            socket: PathBuf::from("/tmp/agent.sock"),
        };
        change(|_| (Some(away), ())).unwrap();
        let looks: Vec<_> = (names.iter().skip(1).cloned())
            .map(|name| {
                std::thread::spawn(move || {
                    change(|away| {
                        let mut away = away.unwrap();
                        // Read, then written a moment later, as a window does
                        // around its daemon's answer.
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        away.leads.remove(&name);
                        (Some(away), ())
                    })
                    .unwrap()
                })
            })
            .collect();
        for look in looks {
            look.join().unwrap();
        }
        let left = read_away().unwrap();
        assert_eq!(left.leads.keys().collect::<Vec<_>>(), [&names[0]]);
        // The last look takes the file with it.
        change(|away| {
            let mut away = away.unwrap();
            away.leads.clear();
            ((!away.leads.is_empty()).then_some(away), ())
        })
        .unwrap();
        assert!(read_away().is_none());
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
