//! The check at a turn's end, in the tool host: no lease, no timer. An
//! agent whose turn completed while it holds work it wrote nothing about
//! that turn is told once for that take, by a `holding` event of the
//! commons addressed to it (`commons::address`). Never on a turn
//! interrupted, failed or paused, on a closed agent, or while it waits on
//! the person's answer to an ask. Work it could take is the board's
//! `opened` news, not this check's. A turn that opened work on a board, however it
//! ended, sends one round of invites for each board at its end, listing
//! the work it opened there. Each thread is read once and again only once its
//! file changes, whichever host wrote it: a turn's end costs a look at
//! each thread's file, as a listing does.
//!
//! A lead's turn's end also gives it its role, its folder's rules and its
//! skills as the person changed them, for its next turn, window or not
//! ([`renew`]).

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
    /// The work each agent's turn opened, by board then thread, in order.
    opened: HashMap<(String, i64), Vec<(String, String)>>,
}

/// What a completed turn's end may tell its agent, before the daemon
/// says it is open.
#[derive(Debug, Default, PartialEq)]
pub struct Found {
    /// The work it still holds and wrote nothing about: thread, title,
    /// board.
    pub holding: Vec<(String, String, String)>,
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

    /// `bot`'s turn `turn` opened `thread` as work on `board`.
    fn opens(&mut self, bot: &str, turn: Option<i64>, board: &str, thread: &str) {
        if let Some(turn) = turn {
            let opened = self.opened.entry((bot.to_owned(), turn)).or_default();
            let it = (board.to_owned(), thread.to_owned());
            if !opened.contains(&it) {
                opened.push(it);
            }
        }
    }

    /// The rounds `bot`'s turn `turn` sends at its end: for each board it
    /// opened work on, in the order it first did, that work, each thread
    /// with its title as it reads now. A turn whose end was never seen
    /// sends none.
    pub fn rounds(&mut self, bot: &str, turn: i64) -> Vec<(String, Vec<(String, String)>)> {
        let opened = (self.opened)
            .remove(&(bot.to_owned(), turn))
            .unwrap_or_default();
        self.opened.retain(|(one, at), _| one != bot || *at > turn);
        let mut rounds: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for (board, thread) in opened {
            let Some((_, Some(work))) = self.threads.get(&thread) else {
                continue;
            };
            let it = (thread, title(work));
            match rounds.iter_mut().find(|(one, _)| *one == board) {
                Some((_, work)) => work.push(it),
                None => rounds.push((board, vec![it])),
            }
        }
        rounds
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
    /// its end may tell it, from the commons as last looked at.
    pub fn ended(&mut self, bot: &str, id: Option<i64>, turn: i64, completed: bool) -> Found {
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
        let mut holding: Vec<(String, String, String)> = (work.iter())
            .filter(|(_, thread)| mine(thread))
            .map(|(name, thread)| {
                let board = thread["work"]["board"].as_str().unwrap_or_default();
                ((*name).clone(), title(thread), board.to_owned())
            })
            .collect();
        holding.sort();
        // What it was told stays told only while it lasts: a take let go,
        // or work that closed and opened again, is news once more.
        let still: HashSet<&String> = holding.iter().map(|(name, _, _)| name).collect();
        self.held
            .retain(|(one, thread)| one != bot || still.contains(thread));
        let held = &self.held;
        holding.retain(|(thread, _, _)| {
            !wrote.contains(thread) && !held.contains(&(bot.to_owned(), thread.clone()))
        });
        Found { holding }
    }

    /// The `holding` news for `bot`, known as `id`, of each work `found`
    /// says it still holds, at the end of its turn `turn`; and each noted
    /// told, so a take is told once.
    pub fn told(
        &mut self,
        bot: &str,
        id: Option<i64>,
        turn: i64,
        found: &Found,
    ) -> Vec<crate::commons::Addressed> {
        let mut news = Vec::new();
        for (thread, title, board) in &found.holding {
            self.held.insert((bot.to_owned(), thread.clone()));
            news.push(holding(bot, id, turn, thread, title, board));
        }
        news
    }
}

/// The `holding` event that tells `bot`, known as `id`, at the end of its
/// turn `turn`, that it still holds `thread`'s work, `title`, on `board`.
fn holding(
    bot: &str,
    id: Option<i64>,
    turn: i64,
    thread: &str,
    title: &str,
    board: &str,
) -> crate::commons::Addressed {
    crate::commons::Addressed {
        event: "holding",
        id: format!("{thread}/holding/{bot}/{turn}"),
        to: bot.to_owned(),
        to_id: id,
        from: None,
        thread: thread.to_owned(),
        board: (!board.is_empty()).then(|| board.to_owned()),
        text: format!(
            "{bot} still holds the work {thread}, \"{title}\", and wrote nothing there this \
             turn: done, fail or drop it, or reply there with what you're waiting on and what \
             will wake you."
        ),
    }
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

/// The board a commons call that went through opened new work on: a
/// post of work, or a reply that made a thread work, which alone names a
/// board (opening work again names none).
pub fn opened_on(arguments: &Value) -> Option<&str> {
    let (post, reply) = (&arguments["post"], &arguments["reply"]);
    match (post["work"] == true, reply["do"] == "open") {
        (true, _) => post["board"].as_str(),
        (_, true) => reply["board"].as_str(),
        _ => None,
    }
}

/// The host's commons call by `bot`, in its turn `turn`, wrote in
/// `thread`, which the commons in `home` now holds as it is, opening it
/// as work on the board `opened` names: read again at once, so a take let
/// go and taken again before the turn ends is a new take. Read with the
/// turn ends free, off the host's runtime.
pub fn wrote(home: &Path, bot: &str, turn: Option<i64>, thread: &str, opened: Option<&str>) {
    let _reading = READING.lock().unwrap();
    let mut ends_now = ends().lock().unwrap();
    ends_now.writes(bot, turn, thread);
    if let Some(board) = opened {
        ends_now.opens(bot, turn, board, thread);
    }
    drop(ends_now);
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

/// The turn `turn` of `bot`, known as `id`, ended as `status`: a completed
/// one may tell it what it holds, once the daemon says it is open and,
/// read from its history, it waits on no answer of the person's: a
/// `holding` event of the commons for each work, which its `to-me`
/// follow hears.
pub fn finished(
    daemon: (std::path::PathBuf, std::path::PathBuf),
    bot: String,
    id: Option<i64>,
    turn: i64,
    status: &str,
) {
    let (store, socket) = daemon;
    if crate::project::is_lead(&bot) {
        renew(socket.clone(), bot.clone(), id);
    }
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let completed = crate::board::on() && status == "completed";
    // The work it opened is asked about however it ended.
    let rounds = ends().lock().unwrap().rounds(&bot, turn);
    if !rounds.is_empty() {
        let (home, bot) = (home.clone(), bot.clone());
        tokio::spawn(async move {
            for (board, opened) in rounds {
                crate::commons::round(&home, &board, &opened, (&bot, Some(turn))).await;
            }
        });
    }
    tokio::spawn(async move {
        let (named, looked) = (bot.clone(), home.clone());
        let found = tokio::task::spawn_blocking(move || {
            if completed {
                scan(&looked);
            }
            ends().lock().unwrap().ended(&named, id, turn, completed)
        })
        .await;
        let Ok(found) = found else { return };
        if found.holding.is_empty() {
            return;
        }
        let Ok((client, _events)) = agent_client::Client::connect(&socket).await else {
            return;
        };
        let listed = client
            .request("bots", json!({"name": bot, "open": true, "limit": 1}))
            .await;
        let open = (listed.ok())
            .and_then(|page| page["bots"].as_array()?.first().cloned())
            .is_some_and(|record| id.is_none_or(|id| record["bot_id"].as_i64() == Some(id)));
        // Its history is read only when there is news, and one it cannot
        // read waits too.
        if !open
            || crate::asks::waits(&client, &bot, now())
                .await
                .unwrap_or(true)
        {
            return;
        }
        let news = ends().lock().unwrap().told(&bot, id, turn, &found);
        let (at, kept) = (home.clone(), store.clone());
        let _ = tokio::task::spawn_blocking(move || {
            for one in &news {
                crate::commons::address(&at, &kept, one);
            }
        })
        .await;
        crate::commons::send_waiting(home, store).await;
    });
}

/// The lead `bot`, known as `id`, ended a turn: when its role, its
/// folder's rules, skills or profiles, or its project's task rule are not
/// what it holds, it is given them as they are now
/// ([`crate::project::changed`]). The daemon keeps them for the end of a
/// turn already running, so they never change within one: its next turn
/// reads them, paying for one call the prompt cache can't serve. Its
/// memory alone changing gives it nothing. Read off the host's runtime:
/// a few small files a lead's turn.
fn renew(socket: std::path::PathBuf, bot: String, id: Option<i64>) {
    tokio::spawn(async move {
        if let Ok((client, _events)) = agent_client::Client::connect(&socket).await {
            let said = renew_on(&client, &bot, id).await;
            if let Some(said) = said {
                println!("{said}");
            }
        }
    });
}

/// What the host found each lead holding at its last look, by its name and
/// identity: an idle lead costs one small entry.
fn known() -> &'static Mutex<HashMap<(String, i64), crate::project::Known>> {
    static KNOWN: OnceLock<Mutex<HashMap<(String, i64), crate::project::Known>>> = OnceLock::new();
    KNOWN.get_or_init(Mutex::default)
}

/// [`renew`] on `client`: the event the host logs, when there is one.
async fn renew_on(client: &agent_client::Client, bot: &str, id: Option<i64>) -> Option<Value> {
    let record = (client)
        .request("inspect", json!({"bot": bot, "bot_id": id}))
        .await
        .ok()?;
    // A lead no agent made: an agent's helper named like one keeps the
    // instructions it was given.
    if !crate::project::is_lead_record(&record) {
        return None;
    }
    let dir = std::path::PathBuf::from(record["workspace"].as_str()?);
    let key = (bot.to_owned(), id.or(record["bot_id"].as_i64())?);
    // What it will hold once a change kept for its turn's end is taken.
    let held = (record["configure_pending"]["instructions"].as_str())
        .or(record["instructions"].as_str())
        .unwrap_or_default()
        .to_owned();
    let chosen = record["profile"].as_str().map(str::to_owned);
    let last = known().lock().unwrap().get(&key).cloned();
    let composed = tokio::task::spawn_blocking(move || {
        crate::project::changed(&dir, &held, chosen.as_deref(), last.as_ref())
    })
    .await
    .ok()?;
    let renewal = match composed {
        Ok(renewal) => renewal,
        Err(error) => {
            return Some(json!({"event": "instructions_unread", "bot": bot, "error": error}));
        }
    };
    let Some(text) = renewal.text else {
        known().lock().unwrap().insert(key, renewal.known);
        return None;
    };
    let asked = json!({"bot": bot, "bot_id": key.1, "instructions": text});
    match client.request("configure", asked).await {
        Ok(_) => {
            known().lock().unwrap().insert(key, renewal.known);
            // As its window would: what it goes without, once a day.
            let _ = crate::project::tell_warnings(client, bot, &renewal.left_out).await;
            Some(json!({"event": "instructions_renewed", "bot": bot}))
        }
        Err(error) => {
            known().lock().unwrap().remove(&key);
            Some(json!({"event": "instructions_renew_failed", "bot": bot, "error": error.code}))
        }
    }
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
        let thread = said["thread"].as_str().unwrap();
        wrote_in(ends, home, name, Some(turn), thread);
        if let Some(board) = opened_on(&arguments) {
            ends.opens(name, Some(turn), board, thread);
        }
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
    ) -> Found {
        let dir = crate::commons::dir(home);
        let here = listing(&dir).unwrap();
        let read = reread(&dir, ends.stale(&here));
        ends.scanned(&here, read);
        ends.ended(bot, id, turn, completed)
    }

    #[test]
    fn a_holder_that_wrote_nothing_is_told_once_for_that_take() {
        let home = home("holding");
        let mut ends = Ends::default();
        send(&home, &mut ends, "shop.lead", 1, work("store", json!({})));
        send(&home, &mut ends, "shop.w.1", 1, act("store", "take"));
        // The turn that took it wrote there: nothing to say.
        let found = end(&mut ends, &home, "shop.w.1", None, 1, true);
        assert!(found.holding.is_empty(), "{found:?}");
        assert!(ends.told("shop.w.1", None, 1, &found).is_empty());
        // A later turn that says nothing there is told, once: a `holding`
        // event of the commons addressed to it, on the work's thread.
        let found = end(&mut ends, &home, "shop.w.1", Some(4), 2, true);
        let held = (
            "store".to_owned(),
            "Do store".to_owned(),
            "shop/errors".to_owned(),
        );
        assert_eq!(found.holding, [held]);
        let news = ends.told("shop.w.1", Some(4), 2, &found);
        assert_eq!(news.len(), 1);
        let line = news[0].line();
        assert_eq!(line["event"], "holding");
        assert_eq!(line["to"], "shop.w.1");
        assert_eq!(line["thread"], "store");
        assert_eq!(line["board"], "shop/errors");
        assert_eq!(line["id"], "store/holding/shop.w.1/2");
        assert!(line.get("from").is_none(), "{line}");
        let text = line["text"].as_str().unwrap();
        assert!(
            text.starts_with("shop.w.1 still holds the work store, \"Do store\""),
            "{text}"
        );
        let found = end(&mut ends, &home, "shop.w.1", Some(4), 3, true);
        assert!(ends.told("shop.w.1", Some(4), 3, &found).is_empty());
        // Interrupted, failed or paused, a turn's end says nothing.
        send(&home, &mut ends, "shop.w.1", 4, act("store", "drop"));
        send(&home, &mut ends, "shop.w.1", 5, act("store", "take"));
        assert_eq!(
            end(&mut ends, &home, "shop.w.1", None, 6, false),
            Found::default()
        );
        // A new take is news again.
        let found = end(&mut ends, &home, "shop.w.1", None, 7, true);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Another agent given a told holder's name, taking its work over, is
    /// told of its own take.
    #[test]
    fn a_take_over_by_another_of_the_holders_name_is_news() {
        let home = home("same-name");
        let mut ends = Ends::default();
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
        let found = end(&mut ends, &home, "shop.w.1", Some(2), 2, true);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        assert_eq!(ends.told("shop.w.1", Some(2), 2, &found).len(), 1);
        take(9, &mut ends);
        let found = end(&mut ends, &home, "shop.w.1", Some(9), 1, true);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Work changed by another host, as one of another store sharing the
    /// commons, is read at the next turn's end all the same.
    #[test]
    fn a_turn_that_opens_work_sends_one_round_a_board_at_its_end() {
        let home = home("rounds");
        let mut ends = Ends::default();
        let other = |name: &str| {
            let mut sent = work(name, json!({}));
            sent["post"]["board"] = json!("shop/docs");
            sent
        };
        send(&home, &mut ends, "shop.lead", 1, work("store", json!({})));
        send(&home, &mut ends, "shop.lead", 1, other("guide"));
        // Waiting on its needs, or kept for some, it is new work all the same.
        let needs = json!({"needs": ["store"], "for": ["shop.w.1"]});
        send(&home, &mut ends, "shop.lead", 1, work("cli", needs));
        // A plain thread made work later in the turn is new work too.
        let plain = json!({"post": {"name": "talk", "title": "Errors talk", "text": "x",
            "board": "shop/errors"}});
        send(&home, &mut ends, "shop.lead", 1, plain);
        let made = json!({"reply": {"thread": "talk", "do": "open", "board": "shop/errors",
            "title": "Type the talk", "text": "y"}});
        send(&home, &mut ends, "shop.lead", 1, made);
        // Another turn's take and talk open nothing.
        send(&home, &mut ends, "shop.w.1", 2, act("store", "take"));
        let pair = |thread: &str, title: &str| (thread.to_owned(), title.to_owned());
        assert_eq!(
            ends.rounds("shop.lead", 1),
            [
                (
                    "shop/errors".to_owned(),
                    vec![
                        pair("store", "Do store"),
                        pair("cli", "Do cli"),
                        pair("talk", "Type the talk")
                    ]
                ),
                ("shop/docs".to_owned(), vec![pair("guide", "Do guide")]),
            ]
        );
        // Sent once, and a work opened again opens no new round.
        assert!(ends.rounds("shop.lead", 1).is_empty());
        assert!(ends.rounds("shop.w.1", 2).is_empty());
        send(&home, &mut ends, "shop.w.1", 3, act("store", "fail"));
        send(&home, &mut ends, "shop.lead", 4, act("store", "open"));
        assert!(ends.rounds("shop.lead", 4).is_empty());
        // A lone piece later is its own round.
        send(&home, &mut ends, "shop.lead", 5, work("docs", json!({})));
        assert_eq!(
            ends.rounds("shop.lead", 5),
            [("shop/errors".to_owned(), vec![pair("docs", "Do docs")])]
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn work_another_host_changed_is_read_at_the_next_turn_s_end() {
        let home = home("other-host");
        let mut ends = Ends::default();
        let found = end(&mut ends, &home, "shop.w.1", None, 1, true);
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
        // Work it could take is the board's `opened` news, not this
        // check's.
        let found = end(&mut ends, &home, "shop.w.1", None, 2, true);
        assert_eq!(found, Found::default());
        send(&home, &mut elsewhere, "shop.w.1", 2, act("store", "take"));
        let found = end(&mut ends, &home, "shop.w.1", None, 3, true);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        assert_eq!(ends.told("shop.w.1", None, 3, &found).len(), 1);
        // Let go there and taken again, even unseen between, it is news
        // once more.
        send(&home, &mut elsewhere, "shop.w.1", 3, act("store", "drop"));
        send(&home, &mut elsewhere, "shop.w.1", 4, act("store", "take"));
        let found = end(&mut ends, &home, "shop.w.1", None, 5, true);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        assert_eq!(ends.told("shop.w.1", None, 5, &found).len(), 1);
        // A reply there is the same take, told of already.
        let reply = json!({"reply": {"thread": "store", "text": "on it"}});
        send(&home, &mut elsewhere, "shop.w.1", 5, reply);
        let found = end(&mut ends, &home, "shop.w.1", None, 6, true);
        assert!(found.holding.is_empty(), "{found:?}");
        // A thread gone is forgotten.
        std::fs::remove_file(crate::commons::dir(&home).join("store.jsonl")).unwrap();
        let found = end(&mut ends, &home, "shop.w.1", None, 7, true);
        assert_eq!(found, Found::default());
        assert!(ends.threads.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A half-written thread is left as it was read, until it changes.
    #[test]
    fn a_thread_that_will_not_read_is_left_as_it_was() {
        let home = home("half");
        let mut ends = Ends::default();
        send(&home, &mut ends, "shop.lead", 1, work("store", json!({})));
        let mut elsewhere = Ends::default();
        send(&home, &mut elsewhere, "shop.w.1", 1, act("store", "take"));
        let found = end(&mut ends, &home, "shop.w.1", None, 1, true);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        let path = crate::commons::dir(&home).join("store.jsonl");
        let whole = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{whole}{{\"by\":")).unwrap();
        let found = end(&mut ends, &home, "shop.w.1", None, 2, true);
        assert_eq!(found.holding.len(), 1, "{found:?}");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn a_lead_s_turn_end_gives_it_a_rule_changed_since_and_nothing_else() {
        let dir = home("renew").join("shop");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Run the tests.\n").unwrap();
        let held = crate::project::changed(&dir, "", None, None).unwrap().text;
        let held = std::sync::Arc::new(Mutex::new(held.unwrap()));
        let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (holds, seen) = (held.clone(), asked.clone());
        let workspace = dir.display().to_string();
        let client = crate::tests::daemon_answering(move |request| {
            seen.lock().unwrap().push(request.clone());
            match request["op"].as_str() {
                Some("inspect") => {
                    let name = request["bot"].as_str().unwrap_or_default();
                    // A helper an agent made, named like a lead.
                    let maker = name.ends_with(".helper").then_some("shop.lead");
                    json!({"name": name, "bot_id": 7, "created_by": maker,
                        "workspace": workspace, "instructions": *holds.lock().unwrap()})
                }
                _ => json!({}),
            }
        })
        .await;
        let configured = || {
            (asked.lock().unwrap().iter())
                .filter(|request| request["op"] == "configure")
                .cloned()
                .collect::<Vec<Value>>()
        };
        // Nothing changed: nothing sent, the lead's cache untouched.
        assert_eq!(renew_on(&client, "shop.lead", Some(7)).await, None);
        assert!(configured().is_empty());
        // The person edits the rules: the lead's next turn reads them.
        std::fs::write(dir.join("AGENTS.md"), "Run the tests twice.\n").unwrap();
        let said = renew_on(&client, "shop.lead", Some(7)).await;
        assert_eq!(said.unwrap()["event"], "instructions_renewed");
        let sent = configured();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["bot_id"], 7);
        let text = sent[0]["instructions"].as_str().unwrap();
        assert!(text.contains("Run the tests twice."), "{text}");
        // An agent's helper named like a lead keeps what it was given.
        assert_eq!(renew_on(&client, "shop.lead.helper", Some(7)).await, None);
        assert_eq!(configured().len(), 1);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
