//! The check at a turn's end, in the tool host: no lease, no timer. An
//! agent whose turn completed while it holds work it wrote nothing about
//! that turn is told once for that take; one of a board's task holding
//! nothing, while work it may take is open there, is told once what is
//! open, and again only when more opens. Never on a turn interrupted,
//! failed or paused, on a closed agent, or while it waits on the person's
//! answer to an ask. The host answers every commons call, so the work it
//! knows is read once and kept as its calls change it.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// What the host keeps between turns.
#[derive(Default)]
pub struct Ends {
    /// Each thread's work, once read, as the host's calls change it.
    work: Option<HashMap<String, Value>>,
    /// The threads each agent wrote in, by its turn.
    wrote: HashMap<(String, i64), HashSet<String>>,
    /// The asks each agent waits on the person's answer to, by its turn
    /// and call.
    asking: HashMap<String, HashSet<(i64, String)>>,
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
    /// The asks it may still wait on.
    pub asking: Vec<(i64, String)>,
}

fn ends() -> &'static Mutex<Ends> {
    static ENDS: OnceLock<Mutex<Ends>> = OnceLock::new();
    ENDS.get_or_init(Mutex::default)
}

impl Ends {
    /// The work as the commons in `home` holds it, read the first time.
    fn work(&mut self, home: &Path) -> &mut HashMap<String, Value> {
        self.work.get_or_insert_with(|| {
            (crate::commons::summaries(&crate::commons::dir(home)).into_iter())
                .filter(|(_, thread)| thread["work"].is_object())
                .collect()
        })
    }

    /// `bot`'s turn `turn` wrote in `thread`, which the commons in `home`
    /// now holds as it is.
    pub fn wrote(&mut self, home: &Path, bot: &str, turn: Option<i64>, thread: &str) {
        if let Some(turn) = turn {
            (self.wrote.entry((bot.to_owned(), turn)).or_default()).insert(thread.to_owned());
        }
        if let Some(work) = self.work.as_mut() {
            let path = crate::commons::dir(home).join(format!("{thread}.jsonl"));
            let summary = crate::commons::summary(thread, &path).filter(|t| t["work"].is_object());
            // A take let go is no longer the one its holder was told of.
            let holder = summary.as_ref().and_then(|t| t["work"]["holder"].as_str());
            (self.held).retain(|(one, at)| at != thread || Some(one.as_str()) == holder);
            match summary {
                Some(summary) => _ = work.insert(thread.to_owned(), summary),
                None => _ = work.remove(thread),
            }
        }
    }

    /// `bot` asked the person in its turn `turn` as call `call`, and waits
    /// for their answer.
    pub fn asked(&mut self, bot: &str, turn: i64, call: &str) {
        (self.asking.entry(bot.to_owned()).or_default()).insert((turn, call.to_owned()));
    }

    /// The ask `call` of `bot`'s turn `turn` is answered or closed.
    pub fn settled(&mut self, bot: &str, turn: i64, call: &str) {
        if let Some(asks) = self.asking.get_mut(bot) {
            asks.remove(&(turn, call.to_owned()));
            if asks.is_empty() {
                self.asking.remove(bot);
            }
        }
    }

    /// `bot`, known as `id`, ended its turn `turn`, completed or not: what
    /// its end may tell it, with `boards` the boards it works from, none
    /// for a lead.
    pub fn ended(
        &mut self,
        home: &Path,
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
        let work = self.work(home);
        let mine = |thread: &Value| {
            let work = &thread["work"];
            work["state"] == "taken"
                && work["holder"] == bot
                && (id.zip(work["holder_id"].as_i64())).is_none_or(|(id, of)| id == of)
        };
        let mut holding: Vec<(String, String)> = (work.iter())
            .filter(|(_, thread)| mine(thread))
            .map(|(name, thread)| (name.clone(), title(thread)))
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
                .map(|(name, thread)| (name.clone(), title(thread), thread["work"]["for"].clone()))
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
        let asking = (self.asking.get(bot).into_iter().flatten())
            .cloned()
            .collect();
        Found {
            holding,
            open,
            asking,
        }
    }

    /// What `bot` is told of `found`, its open work narrowed to what is
    /// for it as `profile`, and noted told: none when nothing is new.
    pub fn told(&mut self, bot: &str, profile: Option<&str>, found: Found) -> Option<String> {
        let open: Vec<(String, String)> = (found.open.into_iter())
            .filter(|(_, _, named)| crate::board::is_for(&json!({"for": named}), bot, profile))
            .map(|(thread, title, _)| (thread, title))
            .collect();
        let known = self.open.entry(bot.to_owned()).or_default();
        let new = open.iter().any(|(thread, _)| !known.contains(thread));
        *known = open.iter().map(|(thread, _)| thread.clone()).collect();
        if known.is_empty() {
            self.open.remove(bot);
        }
        for (thread, _) in &found.holding {
            self.held.insert((bot.to_owned(), thread.clone()));
        }
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
}

fn title(thread: &Value) -> String {
    (thread["work"]["title"].as_str())
        .or(thread["title"].as_str())
        .unwrap_or_default()
        .to_owned()
}

/// The host's commons call by `bot`, in its turn `turn`, wrote in
/// `thread`.
pub fn wrote(home: &Path, bot: &str, turn: Option<i64>, thread: &str) {
    ends().lock().unwrap().wrote(home, bot, turn, thread);
}

/// `bot` asked the person in its turn `turn` as call `call`: one that
/// waits for their answer is noted.
pub fn asked(bot: &str, turn: i64, call: &str, arguments: &Value) {
    let step = (arguments["options"].as_array()).is_none_or(Vec::is_empty);
    if step || arguments["one_way"] == true {
        ends().lock().unwrap().asked(bot, turn, call);
    }
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
/// is open and its asks wait on no one.
pub fn finished(bot: String, id: Option<i64>, turn: i64, status: String) {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let on = crate::board::on();
    tokio::spawn(async move {
        let Ok((store, socket)) = crate::daemon_paths(None, None) else {
            return;
        };
        let (named, at) = (bot.clone(), home.clone());
        let found = tokio::task::spawn_blocking(move || {
            let completed = on && status == "completed";
            let boards = match completed {
                true => boards(&store, &named),
                false => Vec::new(),
            };
            ends()
                .lock()
                .unwrap()
                .ended(&at, &named, id, turn, completed, &boards)
        })
        .await;
        let Ok(found) = found else { return };
        if found.holding.is_empty() && found.open.is_empty() {
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
        for (asked, call) in &found.asking {
            match crate::asks::still_open(&bot, *asked, call).await {
                Ok(false) => ends().lock().unwrap().settled(&bot, *asked, call),
                _ => return,
            }
        }
        let profile = record["profile"].as_str();
        let Some(note) = ends().lock().unwrap().told(&bot, profile, found) else {
            return;
        };
        let submit = json!({"bot": bot, "prompt": note, "delivery": "queue",
            "origin": crate::commons::ORIGIN, "request_id": format!("commons.turn_end.{bot}.{turn}")});
        let _ = client.request("submit", submit).await;
    });
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
        };
        if let Some(thread) = arguments["reply"]["thread"].as_str() {
            let path = crate::commons::dir(home).join(format!("{thread}.jsonl"));
            let posts = crate::commons::posts(&path).unwrap();
            crate::commons::told(home, name, thread, &(0..posts.len()).collect::<Vec<_>>());
        }
        let said = crate::commons::call_as(home, &caller, &arguments, &|_, _| false).unwrap();
        assert_ne!(said["posted"], false, "{said}");
        ends.wrote(home, name, Some(turn), said["thread"].as_str().unwrap());
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

    #[test]
    fn a_holder_that_wrote_nothing_is_told_once_for_that_take() {
        let home = home("holding");
        let mut ends = Ends::default();
        let boards = ["shop/errors".to_owned()];
        send(&home, &mut ends, "shop.lead", 1, work("store", json!({})));
        send(&home, &mut ends, "shop.w.1", 1, act("store", "take"));
        // The turn that took it wrote there: nothing to say.
        let found = ends.ended(&home, "shop.w.1", None, 1, true, &boards);
        assert!(
            found.holding.is_empty() && found.open.is_empty(),
            "{found:?}"
        );
        assert_eq!(ends.told("shop.w.1", None, found), None);
        // A later turn that says nothing there is told, once.
        let found = ends.ended(&home, "shop.w.1", None, 2, true, &boards);
        assert_eq!(found.holding, [("store".to_owned(), "Do store".to_owned())]);
        let note = ends.told("shop.w.1", None, found).unwrap();
        assert!(
            note.contains("You still hold the work store, \"Do store\""),
            "{note}"
        );
        let found = ends.ended(&home, "shop.w.1", None, 3, true, &boards);
        assert_eq!(ends.told("shop.w.1", None, found), None);
        // Interrupted, failed or paused, a turn's end says nothing.
        send(&home, &mut ends, "shop.w.1", 4, act("store", "drop"));
        send(&home, &mut ends, "shop.w.1", 5, act("store", "take"));
        assert_eq!(
            ends.ended(&home, "shop.w.1", None, 6, false, &boards),
            Found::default()
        );
        // A new take is news again.
        let found = ends.ended(&home, "shop.w.1", None, 7, true, &boards);
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
        let found = ends.ended(&home, "shop.w.1", None, 1, true, &boards);
        let note = ends.told("shop.w.1", Some("worker"), found).unwrap();
        assert!(note.contains("store, \"Do store\""), "{note}");
        assert!(!note.contains("api") && !note.contains("docs"), "{note}");
        // Choosing not to take it is not nagged.
        let found = ends.ended(&home, "shop.w.1", None, 2, true, &boards);
        assert_eq!(ends.told("shop.w.1", Some("worker"), found), None);
        // More work opening is news.
        send(&home, &mut ends, "shop.lead", 2, work("cli", json!({})));
        let found = ends.ended(&home, "shop.w.1", None, 3, true, &boards);
        let note = ends.told("shop.w.1", Some("worker"), found).unwrap();
        assert!(note.contains("cli") && note.contains("store"), "{note}");
        // A board it doesn't work from, or a lead's, tells nothing.
        let found = ends.ended(&home, "shop.w.2", None, 1, true, &[]);
        assert_eq!(found, Found::default());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_ask_waiting_on_the_person_is_kept_until_settled() {
        let mut ends = Ends::default();
        ends.asked("shop.w.1", 3, "c1");
        ends.asked("shop.w.1", 4, "c2");
        ends.settled("shop.w.1", 3, "c1");
        assert_eq!(ends.asking["shop.w.1"].len(), 1);
        ends.settled("shop.w.1", 4, "c2");
        assert!(ends.asking.is_empty());
    }
}
