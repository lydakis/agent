//! The check at a turn's end, in the tool host: no lease, no timer. An
//! agent whose turn completed while it holds work it wrote nothing about
//! that turn is told once for that take; one of a board's task holding
//! nothing, while work it may take is open there, is told once what is
//! open, and again only when more opens. Never on a turn interrupted,
//! failed or paused, on a closed agent, or while it waits on the person's
//! answer to an ask. Each thread is read once and again only once its
//! file changes, whichever host wrote it: a turn's end costs a look at
//! each thread's file, as a listing does.

use crate::kept::Stamp;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// What the host keeps between turns.
#[derive(Default)]
pub struct Ends {
    /// Each thread as last read, with its file's stamp then: its summary
    /// when it is work, none when it is not.
    threads: HashMap<String, (Stamp, Option<Value>)>,
    /// The threads each agent wrote in, by its turn.
    wrote: HashMap<(String, i64), HashSet<String>>,
    /// The work each agent was told it still holds, for that take.
    held: HashSet<(String, String)>,
    /// The open work each agent was told of.
    open: HashMap<String, HashSet<String>>,
}

/// What a completed turn's end may tell its agent, before the daemon
/// says it is open and as what profile.
#[derive(Debug, Default, PartialEq)]
pub struct Found {
    /// The work it still holds and wrote nothing about: thread, title.
    pub holding: Vec<(String, String)>,
    /// Holding nothing, the open work of its boards it may take, each
    /// with whom it is for.
    pub open: Vec<(String, String, Value)>,
}

fn ends() -> &'static Mutex<Ends> {
    static ENDS: OnceLock<Mutex<Ends>> = OnceLock::new();
    ENDS.get_or_init(Mutex::default)
}

impl Ends {
    /// `bot`'s turn `turn` wrote in `thread`.
    fn writes(&mut self, bot: &str, turn: Option<i64>, thread: &str) {
        if let Some(turn) = turn {
            (self.wrote.entry((bot.to_owned(), turn)).or_default()).insert(thread.to_owned());
        }
    }

    /// Of the threads `here`, each with its file's stamp, those changed
    /// since read, or never read.
    fn stale(&self, here: &[(String, Stamp)]) -> Vec<(String, Stamp)> {
        (here.iter())
            .filter(|(name, stamp)| (self.threads.get(name)).is_none_or(|(at, _)| at != stamp))
            .cloned()
            .collect()
    }

    /// Takes in a look at the commons: `here` every thread in it, and
    /// `read` those read again, each with its work, if it is work.
    fn scanned(&mut self, here: &[(String, Stamp)], read: Vec<(String, Stamp, Option<Value>)>) {
        let names: HashSet<&str> = here.iter().map(|(name, _)| name.as_str()).collect();
        let gone: Vec<String> = (self.threads.keys())
            .filter(|name| !names.contains(name.as_str()))
            .cloned()
            .collect();
        for thread in gone {
            self.refreshed(&thread, None);
        }
        for (thread, stamp, work) in read {
            self.refreshed(&thread, Some((stamp, work)));
        }
    }

    /// `thread` now reads as `read`, or is gone.
    fn refreshed(&mut self, thread: &str, read: Option<(Stamp, Option<Value>)>) {
        // A take let go is no longer the one its holder was told of, nor
        // one taken over by another agent of its name, nor one let go and
        // taken again since.
        let was = self.threads.get(thread).and_then(|(_, work)| work.as_ref());
        let now = read.as_ref().and_then(|(_, work)| work.as_ref());
        let id = |t: Option<&Value>| t.map(|t| (t["work"]["holder_id"].clone(), t[TAKES].clone()));
        let same = id(was) == id(now);
        let holder = now.and_then(|t| t["work"]["holder"].as_str());
        (self.held).retain(|(one, at)| at != thread || (same && Some(one.as_str()) == holder));
        match read {
            Some(read) => _ = self.threads.insert(thread.to_owned(), read),
            None => _ = self.threads.remove(thread),
        }
    }

    /// `bot`, known as `id`, ended its turn `turn`, completed or not: what
    /// its end may tell it, with `boards` the boards it works from, none
    /// for a lead, from the commons as last looked at.
    pub fn ended(
        &mut self,
        bot: &str,
        id: Option<i64>,
        turn: i64,
        completed: bool,
        boards: &[String],
    ) -> Found {
        let wrote = self
            .wrote
            .remove(&(bot.to_owned(), turn))
            .unwrap_or_default();
        // A turn whose end was never seen writes no more.
        self.wrote.retain(|(one, at), _| one != bot || *at > turn);
        if !completed {
            return Found::default();
        }
        let work: Vec<(&String, &Value)> = (self.threads.iter())
            .filter_map(|(name, (_, work))| Some((name, work.as_ref()?)))
            .collect();
        let mine = |thread: &Value| {
            let work = &thread["work"];
            work["state"] == "taken"
                && work["holder"] == bot
                && (id.zip(work["holder_id"].as_i64())).is_none_or(|(id, of)| id == of)
        };
        let mut holding: Vec<(String, String)> = (work.iter())
            .filter(|(_, thread)| mine(thread))
            .map(|(name, thread)| ((*name).clone(), title(thread)))
            .collect();
        holding.sort();
        let holds = !holding.is_empty();
        let done: HashSet<&str> = (work.iter())
            .filter(|(_, thread)| thread["work"]["state"] == "done")
            .map(|(name, _)| name.as_str())
            .collect();
        let mut open: Vec<(String, String, Value)> = match holds {
            true => Vec::new(),
            false => (work.iter())
                .filter(|(_, thread)| {
                    let work = &thread["work"];
                    work["state"] == "open"
                        && (work["board"].as_str()).is_some_and(|b| boards.iter().any(|o| o == b))
                        && !crate::board::waiting(work, &|need| done.contains(need))
                })
                .map(|(name, thread)| {
                    let named = (*name).clone();
                    (named, title(thread), thread["work"]["for"].clone())
                })
                .collect(),
        };
        open.sort_by(|a, b| a.0.cmp(&b.0));
        // What it was told stays told only while it lasts: a take let go,
        // or work that closed and opened again, is news once more.
        let still: HashSet<&String> = holding.iter().map(|(name, _)| name).collect();
        self.held
            .retain(|(one, thread)| one != bot || still.contains(thread));
        let held = &self.held;
        holding.retain(|(thread, _)| {
            !wrote.contains(thread) && !held.contains(&(bot.to_owned(), thread.clone()))
        });
        Found { holding, open }
    }

    /// What `bot` would be told of `found`, its open work narrowed to
    /// what is for it as `profile`: none when nothing is new.
    pub fn note(&self, bot: &str, profile: Option<&str>, found: &Found) -> Option<String> {
        let open = for_it(bot, profile, found);
        let known = self.open.get(bot);
        let new =
            (open.iter()).any(|(thread, _)| known.is_none_or(|known| !known.contains(thread)));
        let mut lines = Vec::new();
        for (thread, title) in &found.holding {
            lines.push(format!(
                "[You still hold the work {thread}, \"{title}\": done, fail or drop it, or reply \
                 there with what you're waiting on and what will wake you.]"
            ));
        }
        if new {
            let named: Vec<String> = (open.iter())
                .map(|(thread, title)| format!("{thread}, \"{title}\""))
                .collect();
            lines.push(format!(
                "[You hold no work, and work you may take is open: {}. Reply in one with do take \
                 to hold it. You're told again only when more opens.]",
                named.join("; ")
            ));
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    /// What `bot` is told of `found`, as [`Ends::note`] says it, and noted
    /// told.
    pub fn told(&mut self, bot: &str, profile: Option<&str>, found: Found) -> Option<String> {
        let note = self.note(bot, profile, &found);
        let open: HashSet<String> = (for_it(bot, profile, &found).into_iter())
            .map(|(thread, _)| thread)
            .collect();
        match open.is_empty() {
            true => _ = self.open.remove(bot),
            false => _ = self.open.insert(bot.to_owned(), open),
        }
        for (thread, _) in found.holding {
            self.held.insert((bot.to_owned(), thread));
        }
        note
    }
}

/// The open work of `found` that is for `bot` as `profile`: thread, title.
fn for_it(bot: &str, profile: Option<&str>, found: &Found) -> Vec<(String, String)> {
    (found.open.iter())
        .filter(|(_, _, named)| crate::board::is_for(&json!({"for": named}), bot, profile))
        .map(|(thread, title, _)| (thread.clone(), title.clone()))
        .collect()
}

fn title(thread: &Value) -> String {
    (thread["work"]["title"].as_str())
        .or(thread["title"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Held while the commons is looked at for the turn ends, one look at a
/// time, so an older look never lands after a newer one.
static READING: Mutex<()> = Mutex::new(());

/// Each thread's file in the commons `dir`, with its stamp: none when the
/// folder cannot be read, and no thread when there is none.
fn listing(dir: &Path) -> Option<Vec<(String, Stamp)>> {
    let entries = match std::fs::read_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        entries => entries.ok()?,
    };
    let here = entries.flatten().filter_map(|entry| {
        let path = entry.path();
        let name = (path.file_name().and_then(|n| n.to_str()))
            .and_then(|n| n.strip_suffix(".jsonl"))
            .filter(|n| crate::commons::valid_name(n).is_ok())?
            .to_owned();
        Some((name, Stamp::of(&entry.metadata().ok()?)?))
    });
    Some(here.collect())
}

/// How a take is written in a thread's file.
const TAKE: &[u8] = br#""do":"take""#;
/// Where a thread as kept here counts its takes.
const TAKES: &str = "takes";

/// The threads `stale` read again from the commons `dir`, each with its
/// work, if it is work. One that will not read, as one half written, is
/// left as it was, to be read once it changes again.
fn reread(dir: &Path, stale: Vec<(String, Stamp)>) -> Vec<(String, Stamp, Option<Value>)> {
    (stale.into_iter())
        .filter_map(|(name, stamp)| {
            let text = std::fs::read(dir.join(format!("{name}.jsonl"))).ok()?;
            let mut thread = crate::commons::summary_of(&name, &text)?;
            // Which take it is: the takes so far, each a line marked so,
            // as a post's text holds its quotes escaped.
            let takes = (text.split(|b| *b == b'\n'))
                .filter(|line| line.windows(TAKE.len()).any(|w| w == TAKE))
                .count();
            thread[TAKES] = json!(takes);
            let work = thread["work"].is_object().then_some(thread);
            Some((name, stamp, work))
        })
        .collect()
}

/// The commons in `home` looked at, its threads read again where they
/// changed, whichever host wrote them; read with the turn ends free, so
/// what waits on them meanwhile never waits on the files.
fn scan(home: &Path) {
    let _reading = READING.lock().unwrap();
    let dir = crate::commons::dir(home);
    let Some(here) = listing(&dir) else {
        return;
    };
    let stale = ends().lock().unwrap().stale(&here);
    let read = reread(&dir, stale);
    ends().lock().unwrap().scanned(&here, read);
}

/// The host's commons call by `bot`, in its turn `turn`, wrote in
/// `thread`, which the commons in `home` now holds as it is: read again
/// at once, so a take let go and taken again before the turn ends is a
/// new take. Read with the turn ends free, off the host's runtime.
pub fn wrote(home: &Path, bot: &str, turn: Option<i64>, thread: &str) {
    let _reading = READING.lock().unwrap();
    ends().lock().unwrap().writes(bot, turn, thread);
    let dir = crate::commons::dir(home);
    let read = match crate::kept::stamp(&dir.join(format!("{thread}.jsonl"))) {
        Some(stamp) => match reread(&dir, vec![(thread.to_owned(), stamp)]).pop() {
            Some((_, stamp, work)) => Some((stamp, work)),
            None => return,
        },
        None => None,
    };
    ends().lock().unwrap().refreshed(thread, read);
}

/// The boards `bot` works from: those it follows, none for a lead.
fn boards(store: &Path, bot: &str) -> Vec<String> {
    if crate::project::is_lead(bot) {
        return Vec::new();
    }
    (crate::lookout::subs::read(store, bot).into_iter())
        .filter(|sub| sub.name.starts_with("board."))
        .filter_map(|sub| sub.filter)
        .flat_map(|filter| {
            (filter["board"].as_array().into_iter().flatten())
                .filter_map(|b| b.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The turn `turn` of `bot`, known as `id`, ended as `status`: a completed
/// one may tell it what it holds or what is open, once the daemon says it
/// is open and, read from its history, it waits on no answer of the
/// person's.
pub fn finished(
    daemon: (std::path::PathBuf, std::path::PathBuf),
    bot: String,
    id: Option<i64>,
    turn: i64,
    status: &str,
) {
    let (store, socket) = daemon;
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let completed = crate::board::on() && status == "completed";
    tokio::spawn(async move {
        let named = bot.clone();
        let found = tokio::task::spawn_blocking(move || {
            let boards = match completed {
                true => {
                    scan(&home);
                    boards(&store, &named)
                }
                false => Vec::new(),
            };
            ends()
                .lock()
                .unwrap()
                .ended(&named, id, turn, completed, &boards)
        })
        .await;
        let Ok(found) = found else { return };
        if found.holding.is_empty() && found.open.is_empty() {
            // Nothing open now: what it was told of is forgotten, so work
            // that closes and opens again is news once more.
            if completed {
                ends().lock().unwrap().told(&bot, None, found);
            }
            return;
        }
        let Ok((client, _events)) = agent_client::Client::connect(&socket).await else {
            return;
        };
        let listed = client
            .request("bots", json!({"name": bot, "open": true, "limit": 1}))
            .await;
        let Some(record) = (listed.ok())
            .and_then(|page| page["bots"].as_array()?.first().cloned())
            .filter(|record| id.is_none_or(|id| record["bot_id"].as_i64() == Some(id)))
        else {
            return;
        };
        let profile = record["profile"].as_str();
        // Only what would be said is held for an answer: its history is
        // read then, and one it cannot read holds it too.
        let news = ends().lock().unwrap().note(&bot, profile, &found).is_some();
        if news
            && crate::asks::waits(&client, &bot, now())
                .await
                .unwrap_or(true)
        {
            return;
        }
        let Some(note) = ends().lock().unwrap().told(&bot, profile, found) else {
            return;
        };
        let submit = json!({"bot": bot, "prompt": note, "delivery": "queue",
            "origin": crate::commons::ORIGIN, "request_id": crate::commons::request_id(&format!("commons.turn_end.{bot}.{turn}"))});
        let _ = client.request("submit", submit).await;
    });
}

/// Seconds since the epoch.
fn now() -> i64 {
    (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH))
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Caller;

    fn home(name: &str) -> std::path::PathBuf {
        let nanos = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH))
            .map_or(0, |at| at.as_nanos());
        let home = std::env::temp_dir().join(format!(
            "sonny-turn-end-{name}-{}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(crate::commons::dir(&home)).unwrap();
        home
    }

    fn send(home: &Path, ends: &mut Ends, name: &str, turn: i64, arguments: Value) -> Value {
        let caller = Caller {
            name,
            id: None,
            reader: name,
            model: None,
            project: Some("shop"),
            profile: None,
            lead: name.ends_with(".lead"),
            folder: None,
        };
        if let Some(thread) = arguments["reply"]["thread"].as_str() {
            let path = crate::commons::dir(home).join(format!("{thread}.jsonl"));
            let posts = crate::commons::posts(&path).unwrap();
            crate::commons::told(home, name, thread, &(0..posts.len()).collect::<Vec<_>>());
        }
        let said = crate::commons::call_as(home, &caller, &arguments, &|_, _| false).unwrap();
        assert_ne!(said["posted"], false, "{said}");
        wrote_in(
            ends,
            home,
            name,
            Some(turn),
            said["thread"].as_str().unwrap(),
        );
        said
    }

    fn work(name: &str, extra: Value) -> Value {
        let mut post = json!({"name": name, "title": format!("Do {name}"), "text": "x",
            "board": "shop/errors", "work": true});
        for (key, value) in extra.as_object().into_iter().flatten() {
            post[key] = value.clone();
        }
        json!({"post": post})
    }

    fn act(thread: &str, what: &str) -> Value {
        json!({"reply": {"thread": thread, "do": what, "text": "y"}})
    }

    /// [`wrote`], into `ends`.
    fn wrote_in(ends: &mut Ends, home: &Path, bot: &str, turn: Option<i64>, thread: &str) {
        ends.writes(bot, turn, thread);
        let dir = crate::commons::dir(home);
        let stamp = crate::kept::stamp(&dir.join(format!("{thread}.jsonl"))).unwrap();
        let (_, stamp, work) = reread(&dir, vec![(thread.to_owned(), stamp)])
            .pop()
            .unwrap();
        ends.refreshed(thread, Some((stamp, work)));
    }

    /// A turn's end after a look at the commons in `home`, as the host's.
    fn end(
        ends: &mut Ends,
        home: &Path,
        bot: &str,
        id: Option<i64>,
        turn: i64,
        completed: bool,
        boards: &[String],
    ) -> Found {
        let dir = crate::commons::dir(home);
        let here = listing(&dir).unwrap();
        let read = reread(&dir, ends.stale(&here));
        ends.scanned(&here, read);
        ends.ended(bot, id, turn, completed, boards)
    }

    #[test]
    fn a_holder_that_wrote_nothing_is_told_once_for_that_take() {
        let home = home("holding");
        let mut ends = Ends::default();
        let boards = ["shop/errors".to_owned()];
        send(&home, &mut ends, "shop.lead", 1, work("store", json!({})));
        send(&home, &mut ends, "shop.w.1", 1, act("store", "take"));
        // The turn that took it wrote there: nothing to say.
        let found = end(&mut ends, &home, "shop.w.1", None, 1, true, &boards);
        assert!(
            found.holding.is_empty() && found.open.is_empty(),
            "{found:?}"
        );
        assert_eq!(ends.told("shop.w.1", None, found), None);
        // A later turn that says nothing there is told, once.
        let found = end(&mut ends, &home, "shop.w.1", None, 2, true, &boards);
        assert_eq!(found.holding, [("store".to_owned(), "Do store".to_owned())]);
        let note = ends.told("shop.w.1", None, found).unwrap();
        assert!(
            note.contains("You still hold the work store, \"Do store\""),
            "{note}"
        );
        let found = end(&mut ends, &home, "shop.w.1", None, 3, true, &boards);
        assert_eq!(ends.told("shop.w.1", None, found), None);
        // Interrupted, failed or paused, a turn's end says nothing.
        send(&home, &mut ends, "shop.w.1", 4, act("store", "drop"));
        send(&home, &mut ends, "shop.w.1", 5, act("store", "take"));
        assert_eq!(
            end(&mut ends, &home, "shop.w.1", None, 6, false, &boards),
            Found::default()
        );
        // A new take is news again.
        let found = end(&mut ends, &home, "shop.w.1", None, 7, true, &boards);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Another agent given a told holder's name, taking its work over, is
    /// told of its own take.
    #[test]
    fn a_take_over_by_another_of_the_holders_name_is_news() {
        let home = home("same-name");
        let mut ends = Ends::default();
        let boards = ["shop/errors".to_owned()];
        send(&home, &mut ends, "shop.lead", 1, work("store", json!({})));
        let take = |id: i64, ends: &mut Ends| {
            let caller = Caller {
                name: "shop.w.1",
                id: Some(id),
                reader: "shop.w.1",
                model: None,
                project: Some("shop"),
                profile: None,
                lead: false,
                folder: None,
            };
            let gone = |_: &str, at: Option<i64>| at != Some(id);
            let path = crate::commons::dir(&home).join("store.jsonl");
            let seen = crate::commons::posts(&path).unwrap().len();
            crate::commons::told(&home, "shop.w.1", "store", &(0..seen).collect::<Vec<_>>());
            let said = crate::commons::call_as(&home, &caller, &act("store", "take"), &gone);
            let said = said.unwrap();
            assert_ne!(said["posted"], false, "{said}");
            wrote_in(
                ends,
                &home,
                "shop.w.1",
                None,
                said["thread"].as_str().unwrap(),
            );
        };
        take(2, &mut ends);
        let found = end(&mut ends, &home, "shop.w.1", Some(2), 2, true, &boards);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        assert!(ends.told("shop.w.1", None, found).is_some());
        take(9, &mut ends);
        let found = end(&mut ends, &home, "shop.w.1", Some(9), 1, true, &boards);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn one_holding_nothing_hears_what_is_open_for_it_until_more_opens() {
        let home = home("open");
        let mut ends = Ends::default();
        let boards = ["shop/errors".to_owned()];
        send(&home, &mut ends, "shop.lead", 1, work("store", json!({})));
        send(
            &home,
            &mut ends,
            "shop.lead",
            1,
            work("api", json!({"needs": ["store"]})),
        );
        send(
            &home,
            &mut ends,
            "shop.lead",
            1,
            work("docs", json!({"for": ["profile:docs"]})),
        );
        // Waiting work and work kept for others are not its to take.
        let found = end(&mut ends, &home, "shop.w.1", None, 1, true, &boards);
        let note = ends.told("shop.w.1", Some("worker"), found).unwrap();
        assert!(note.contains("store, \"Do store\""), "{note}");
        assert!(!note.contains("api") && !note.contains("docs"), "{note}");
        // Choosing not to take it is not nagged.
        let found = end(&mut ends, &home, "shop.w.1", None, 2, true, &boards);
        assert_eq!(ends.told("shop.w.1", Some("worker"), found), None);
        // More work opening is news.
        send(&home, &mut ends, "shop.lead", 2, work("cli", json!({})));
        let found = end(&mut ends, &home, "shop.w.1", None, 3, true, &boards);
        let note = ends.told("shop.w.1", Some("worker"), found).unwrap();
        assert!(note.contains("cli") && note.contains("store"), "{note}");
        // A board it doesn't work from, or a lead's, tells nothing.
        let found = end(&mut ends, &home, "shop.w.2", None, 1, true, &[]);
        assert_eq!(found, Found::default());
        // Work others took leaves nothing open, which is told of as
        // nothing, as a turn's end does: once let go, it is news again.
        for thread in ["store", "cli"] {
            send(&home, &mut ends, "shop.w.3", 1, act(thread, "take"));
        }
        let found = end(&mut ends, &home, "shop.w.1", None, 4, true, &boards);
        assert_eq!(ends.told("shop.w.1", Some("worker"), found), None);
        send(&home, &mut ends, "shop.w.3", 2, act("store", "drop"));
        let found = end(&mut ends, &home, "shop.w.1", None, 5, true, &boards);
        let note = ends.told("shop.w.1", Some("worker"), found).unwrap();
        assert!(note.contains("store"), "{note}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Work changed by another host, as one of another store sharing the
    /// commons, is read at the next turn's end all the same.
    #[test]
    fn work_another_host_changed_is_read_at_the_next_turn_s_end() {
        let home = home("other-host");
        let mut ends = Ends::default();
        let boards = ["shop/errors".to_owned()];
        let found = end(&mut ends, &home, "shop.w.1", None, 1, true, &boards);
        assert_eq!(found, Found::default());
        // Written with no word to this host.
        let mut elsewhere = Ends::default();
        send(
            &home,
            &mut elsewhere,
            "shop.lead",
            1,
            work("store", json!({})),
        );
        let found = end(&mut ends, &home, "shop.w.1", None, 2, true, &boards);
        assert_eq!(found.open.len(), 1, "{found:?}");
        send(&home, &mut elsewhere, "shop.w.1", 2, act("store", "take"));
        let found = end(&mut ends, &home, "shop.w.1", None, 3, true, &boards);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        assert!(ends.told("shop.w.1", None, found).is_some());
        // Let go there and taken again, even unseen between, it is news
        // once more.
        send(&home, &mut elsewhere, "shop.w.1", 3, act("store", "drop"));
        send(&home, &mut elsewhere, "shop.w.1", 4, act("store", "take"));
        let found = end(&mut ends, &home, "shop.w.1", None, 5, true, &boards);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        assert!(ends.told("shop.w.1", None, found).is_some());
        // A reply there is the same take, told of already.
        let reply = json!({"reply": {"thread": "store", "text": "on it"}});
        send(&home, &mut elsewhere, "shop.w.1", 5, reply);
        let found = end(&mut ends, &home, "shop.w.1", None, 6, true, &boards);
        assert!(found.holding.is_empty(), "{found:?}");
        // A thread gone is forgotten.
        std::fs::remove_file(crate::commons::dir(&home).join("store.jsonl")).unwrap();
        let found = end(&mut ends, &home, "shop.w.1", None, 7, true, &boards);
        assert_eq!(found, Found::default());
        assert!(ends.threads.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A half-written thread is left as it was read, until it changes.
    #[test]
    fn a_thread_that_will_not_read_is_left_as_it_was() {
        let home = home("half");
        let mut ends = Ends::default();
        let boards = ["shop/errors".to_owned()];
        send(&home, &mut ends, "shop.lead", 1, work("store", json!({})));
        let found = end(&mut ends, &home, "shop.w.1", None, 1, true, &boards);
        assert_eq!(found.open.len(), 1, "{found:?}");
        let path = crate::commons::dir(&home).join("store.jsonl");
        let whole = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{whole}{{\"by\":")).unwrap();
        let found = end(&mut ends, &home, "shop.w.1", None, 2, true, &boards);
        assert_eq!(found.open.len(), 1, "{found:?}");
        let _ = std::fs::remove_dir_all(&home);
    }
}
