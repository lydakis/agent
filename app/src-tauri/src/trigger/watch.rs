//! The turn-end watcher: one process, while any trigger waits on an agent's
//! turn ends, for all of them. It connects once per daemon and follows each
//! of those agents' events; between turn ends it waits on the socket, and
//! with no daemon there it tries again, at most once a minute. A turn end
//! that counts is an ask in the trigger's queue, saying why, for which
//! launchd runs its fire, as for `fire NAME`; the watcher runs nothing
//! itself. Where it is in each agent's events is kept per trigger
//! (`NAME.watch`), once the ask is on disk, so a restart misses no turn end.
//! An ask is named by its turn end's place in those events, so one asked
//! again by a watcher stopped before it saved is the same ask, or one its
//! fire already took (`take_ask`): none is sent twice. A turn a trigger sent
//! that has not ended keeps the place it is read again from before it, so a
//! restart still knows it for that trigger's.
use super::*;
use std::collections::HashMap;

const MAX_WAIT: Duration = Duration::from_secs(60);
/// Turns a trigger sent that have not ended yet; more is a leak, not work.
const MAX_SENT: usize = 1024;

/// One turn-end trigger, as the watcher holds it.
struct Watched {
    trigger: Trigger,
    /// The last event of its agent it has counted, or passed over.
    cursor: Option<i64>,
    /// Where its agent's events are read again from after a restart: before
    /// any turn a trigger sent that has not ended, else its cursor.
    from: Option<i64>,
    /// Turn ends counted since it last fired.
    count: u64,
    done: bool,
}

impl Watched {
    fn source(&self) -> &str {
        self.trigger.turn_end.as_ref().map_or("", |(bot, _)| bot)
    }
    fn source_id(&self) -> Option<i64> {
        self.trigger.turn_end.as_ref().map(|(_, id)| *id)
    }
}

/// What an event asks of the watcher, by trigger.
#[derive(Debug, PartialEq)]
enum Step {
    /// Keep where it is.
    Save(usize),
    /// Keep where it is and fire, saying why.
    Fire(usize, String),
    /// Its agent is gone: it ends.
    Gone(usize, String),
    /// Retention removed events it would read: its last result says so,
    /// and it goes on past them.
    Gap(usize, String),
}

/// The `request_id`s of turns triggers sent, by agent and turn, from their
/// `accepted` or `queued` events, and where the first of those events is,
/// until those turns end.
type Sent = HashMap<(String, i64), (String, i64)>;

/// What one event means for the triggers following its agent. Only a turn
/// that ends after a trigger's cursor counts, and not one the trigger sent.
fn step(watched: &mut [Watched], sent: &mut Sent, event: &Value) -> Vec<Step> {
    let (Some(bot), Some(kind)) = (event["bot"].as_str(), event["event"].as_str()) else {
        return Vec::new();
    };
    let mut steps = Vec::new();
    match kind {
        // A queued turn can end without starting (stopped, or taken in as
        // a steer), so it is known from either.
        "accepted" | "queued" => {
            if let (Some(turn), Some(id)) =
                (event["turn"].as_i64(), event["data"]["request_id"].as_str())
                && id.starts_with("trigger_")
            {
                // An agent runs one turn at a time, so this holds about one
                // per agent followed; past the bound, the oldest turns go,
                // whose ends were lost with a connection.
                if sent.len() >= MAX_SENT {
                    let mut turns: Vec<i64> = sent.keys().map(|(_, t)| *t).collect();
                    turns.sort_unstable();
                    let oldest = turns[MAX_SENT / 2];
                    sent.retain(|(_, t), _| *t >= oldest);
                }
                let at = event["cursor"].as_i64().unwrap_or(0);
                sent.entry((bot.to_owned(), turn))
                    .or_insert_with(|| (id.to_owned(), at));
            }
        }
        // Deleted while followed: its triggers end.
        "deleted" => {
            for (i, w) in watched.iter_mut().enumerate() {
                if !w.done && w.source() == bot {
                    w.done = true;
                    steps.push(Step::Gone(i, format!("bot_not_found: {bot} was deleted")));
                }
            }
        }
        // Gone from where it reads again from: turn ends it had not
        // counted, or the start of a turn a trigger sent that has not ended.
        // What is gone is passed over, so it is said once.
        "pruned" => {
            let before = event["before"].as_i64().unwrap_or(0);
            let why = format!(
                "events_pruned: events of {bot} through {before} were removed before they were \
                 read: turn ends among them went uncounted, and one of a turn this trigger sent \
                 there may count"
            );
            for (i, w) in watched.iter_mut().enumerate() {
                let from = w.from.or(w.cursor);
                if !w.done && w.source() == bot && from.is_none_or(|c| c < before) {
                    w.cursor = w.cursor.max(Some(before));
                    steps.push(Step::Gap(i, why.clone()));
                }
            }
        }
        // The name is another agent's now: the one it was made for is gone.
        "created" => {
            for (i, w) in watched.iter_mut().enumerate() {
                if !w.done && w.source() == bot && event["data"]["id"].as_i64() != w.source_id() {
                    w.done = true;
                    steps.push(Step::Gone(
                        i,
                        format!("bot_not_found: {bot} is another agent now"),
                    ));
                }
            }
        }
        "turn_finished" => {
            let (Some(cursor), Some(turn)) = (event["cursor"].as_i64(), event["turn"].as_i64())
            else {
                return steps;
            };
            let by = sent.remove(&(bot.to_owned(), turn)).map(|(id, _)| id);
            for (i, w) in watched.iter_mut().enumerate() {
                if w.done || w.source() != bot || w.cursor.is_some_and(|c| cursor <= c) {
                    continue;
                }
                w.cursor = Some(cursor);
                if by
                    .as_deref()
                    .is_some_and(|id| id.starts_with(&sent_by(&w.trigger)))
                {
                    steps.push(Step::Save(i));
                    continue;
                }
                w.count += 1;
                if w.count < w.trigger.count.unwrap_or(1) {
                    steps.push(Step::Save(i));
                    continue;
                }
                w.count = 0;
                let status = event["data"]["status"].as_str().unwrap_or("ended");
                steps.push(Step::Fire(
                    i,
                    format!("{}: turn:{bot}/{turn} {status}", w.trigger.when),
                ));
            }
        }
        _ => {}
    }
    steps
}

/// `APP --trigger-watch`, launchd's job while any turn-end trigger is
/// there. It exits 0 once none is, which launchd leaves stopped.
pub fn watch_cli() -> i32 {
    let places = match Places::home() {
        Ok(places) => places,
        Err(error) => {
            eprintln!("{}", error_json(&error));
            return 1;
        }
    };
    let mut daemons: HashMap<(PathBuf, String), Vec<Watched>> = HashMap::new();
    for (trigger, _) in read_all(&places).filter_map(|(_, read)| read.ok()) {
        if trigger.turn_end.is_none() {
            continue;
        }
        let Ok(socket) = trigger.daemon.socket() else {
            continue;
        };
        let place = match read_watched(&places, &trigger) {
            Ok(place) => place,
            // Its place lost, it cannot know which turn ends it has counted.
            Err(error) => {
                stops(&places, &trigger, error, &launchctl);
                continue;
            }
        };
        daemons
            .entry((socket, trigger.store_id.clone()))
            .or_default()
            .push(Watched {
                trigger,
                cursor: place.map(|p| p.cursor),
                from: place.map(|p| p.from),
                count: place.map_or(0, |p| p.count),
                done: false,
            });
    }
    if daemons.is_empty() {
        return 0;
    }
    let runtime = match runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{}", error_json(&error));
            return 1;
        }
    };
    runtime.block_on(async {
        let mut follows = tokio::task::JoinSet::new();
        for ((socket, store), watched) in daemons {
            follows.spawn(follow(places.clone(), socket, store, watched));
        }
        while follows.join_next().await.is_some() {}
    });
    0
}

/// Follow one daemon's agents for its triggers until none is left.
async fn follow(places: Places, socket: PathBuf, store: String, mut watched: Vec<Watched>) {
    let log = |error: String| eprintln!("{}", error_json(&error));
    let mut sent = Sent::new();
    let mut wait = Duration::from_secs(1);
    loop {
        if watched.iter().all(|w| w.done) {
            return;
        }
        // A daemon on the socket serving another store is not theirs.
        let connected = match Client::connect(&socket).await {
            Ok((client, events)) if client.store() == Some(store.as_str()) => {
                Some((client, events))
            }
            Ok((client, _)) => {
                client.close().await;
                None
            }
            Err(_) => None,
        };
        let Some((client, mut events)) = connected else {
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(MAX_WAIT);
            continue;
        };
        wait = Duration::from_secs(1);
        let from = match begin(&places, &client, &mut watched).await {
            Ok(from) => from,
            Err(error) => {
                log(error);
                client.close().await;
                tokio::time::sleep(MAX_WAIT).await;
                continue;
            }
        };
        // Its events are read while the follows go out: what they replay
        // can be more than the client holds unread.
        let mut following = tokio::spawn({
            let client = client.clone();
            async move {
                for (bot, after) in from {
                    client
                        .request("follow", json!({"bot": bot, "after": after}))
                        .await
                        .map_err(super::coded)?;
                }
                Ok::<(), String>(())
            }
        });
        let (mut kept, mut followed) = (true, false);
        loop {
            tokio::select! {
                done = &mut following, if !followed => {
                    followed = true;
                    if let Err(error) = done.map_err(|e| e.to_string()).and_then(|r| r) {
                        log(error);
                        kept = false;
                        break;
                    }
                }
                event = events.recv() => {
                    let Some(event) = event else { break };
                    for step in step(&mut watched, &mut sent, &event) {
                        kept &= act(&places, &mut watched, &sent, step);
                    }
                    if !kept || watched.iter().all(|w| w.done) {
                        break;
                    }
                }
            }
        }
        following.abort();
        client.close().await;
        // An ask not made, or a place not saved: counts and places go back
        // to what is on disk, and following again from there reads those
        // turn ends again.
        if !kept {
            for w in watched.iter_mut().filter(|w| !w.done) {
                match read_watched(&places, &w.trigger) {
                    Ok(Some(place)) => {
                        (w.cursor, w.from, w.count) =
                            (Some(place.cursor), Some(place.from), place.count);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        w.done = true;
                        stops(&places, &w.trigger, error, &launchctl);
                    }
                }
            }
            tokio::time::sleep(wait).await;
        }
    }
}

/// On each connection: end the triggers whose agent is gone, find where
/// a new one starts, and say where to follow each agent from: the earliest
/// place any of its triggers is at. Each agent is looked up once, however
/// many triggers follow it.
async fn begin(
    places: &Places,
    client: &Client,
    watched: &mut [Watched],
) -> Result<Vec<(String, i64)>, String> {
    let mut ids = HashMap::<String, Option<i64>>::new();
    let mut newest = HashMap::<String, i64>::new();
    for w in watched.iter_mut().filter(|w| !w.done) {
        let source = w.source().to_owned();
        let now = match ids.get(&source) {
            Some(id) => *id,
            None => {
                let id = match bot_id(client, &source).await {
                    Ok(id) => Some(id),
                    Err(error) if error.starts_with("bot_not_found") => None,
                    Err(error) => return Err(error),
                };
                *ids.entry(source.clone()).or_insert(id)
            }
        };
        if now != w.source_id() {
            let gone = json!({"outcome": "gone", "detail": format!("bot_not_found: {} is gone", w.source())});
            // Done once that is on disk and the trigger gone; else it is
            // looked at again on the next connection.
            w.done = settle(
                places,
                &w.trigger,
                &gone,
                &Kept::of(&state(places, &w.trigger)),
                &launchctl,
            );
            if !w.done {
                return Err(format!("{}: its end was not recorded", w.trigger.name));
            }
            continue;
        }
        if w.cursor.is_none() {
            let at = match newest.get(&source) {
                Some(at) => *at,
                None => *newest
                    .entry(source.clone())
                    .or_insert(newest_cursor(client, &source).await?),
            };
            w.cursor = Some(at);
            save(places, w, &Sent::new())?;
        }
    }
    let mut from = HashMap::<String, i64>::new();
    for w in watched.iter().filter(|w| !w.done) {
        let at = w.from.or(w.cursor).unwrap_or(0);
        from.entry(w.source().to_owned())
            .and_modify(|c| *c = (*c).min(at))
            .or_insert(at);
    }
    Ok(from.into_iter().collect())
}

/// Do one step; false when an ask could not be made, a place saved, or an
/// end or gap recorded, so the watcher reads those events again from where
/// it last saved.
fn act(places: &Places, watched: &mut [Watched], sent: &Sent, step: Step) -> bool {
    let log = |error: String| {
        eprintln!("{}", error_json(&error));
        false
    };
    match step {
        Step::Save(i) => save(places, &mut watched[i], sent).map_or_else(log, |()| true),
        // Done once its end is on disk and the trigger gone.
        Step::Gone(i, detail) => {
            let w = &mut watched[i];
            let gone = json!({"outcome": "gone", "detail": detail});
            let kept = Kept::of(&state(places, &w.trigger));
            w.done = settle(places, &w.trigger, &gone, &kept, &launchctl);
            w.done
        }
        // A gap ends nothing: the trigger goes on from past it.
        Step::Gap(i, detail) => {
            let w = &mut watched[i];
            let said = gap(places, w, &detail).and_then(|()| match w.done {
                false => save(places, w, sent),
                true => Ok(()),
            });
            said.map_or_else(log, |()| true)
        }
        Step::Fire(i, why) => {
            let w = &mut watched[i];
            // Where it is goes on disk only once the ask is: a watcher
            // stopped before then asks again, which is the same ask.
            let asked = ask_fire(places, w, &why).and_then(|()| match w.done {
                false => save(places, w, sent),
                true => Ok(()),
            });
            asked.map_or_else(log, |()| true)
        }
    }
}

/// Ask for the trigger's fire, saying why, under the lock `rm` takes, and
/// only while its plist is still this trigger's: one ended (its runs, `rm`)
/// or replaced is done. launchd runs the fire; one still running, as on a
/// long `--reply-to` wait, finishes first, and launchd runs it again.
fn ask_fire(places: &Places, w: &mut Watched, why: &str) -> Result<(), String> {
    let _lock = Lock::take(places)?;
    if !ours(places, &w.trigger) {
        w.done = true;
        return Ok(());
    }
    ask_turn(places, &w.trigger.name, w.cursor.unwrap_or(0), why)
}

/// Say in the trigger's last result that events it would read are gone,
/// keeping the rest of what its fires left (a message one began), under the
/// lock and only while its plist is still this trigger's.
fn gap(places: &Places, w: &mut Watched, detail: &str) -> Result<(), String> {
    let _lock = Lock::take(places)?;
    if !ours(places, &w.trigger) {
        w.done = true;
        return Ok(());
    }
    let failed = json!({"outcome": "failed", "detail": detail});
    record_last(
        places,
        &w.trigger,
        &failed,
        &Kept::of(&state(places, &w.trigger)),
    )
}

/// Keep where it is, and where to read again from: before the first event
/// of a turn a trigger sent that has not ended, whose end would otherwise
/// be read without knowing whose it was. Under the lock `rm` takes, and
/// only while its plist is still this trigger's: a place saved after the
/// trigger went would be left behind.
fn save(places: &Places, w: &mut Watched, sent: &Sent) -> Result<(), String> {
    let _lock = Lock::take(places)?;
    if !ours(places, &w.trigger) {
        w.done = true;
        return Ok(());
    }
    let cursor = w.cursor.unwrap_or(0);
    let from = sent
        .iter()
        .filter(|((bot, _), _)| bot == w.source())
        .map(|(_, (_, at))| at - 1)
        .fold(cursor, i64::min);
    w.from = Some(from);
    save_watched(
        places,
        &w.trigger,
        Place {
            cursor,
            count: w.count,
            from,
        },
    )
}

/// Where the watcher is for a trigger, as it keeps it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Place {
    /// The last event counted or passed over.
    pub cursor: i64,
    /// Turn ends counted since it last fired.
    pub count: u64,
    /// Where to read its agent's events again from.
    pub from: i64,
}

pub(super) fn save_watched(places: &Places, trigger: &Trigger, place: Place) -> Result<(), String> {
    let kept = json!({"generation": trigger.generation, "cursor": place.cursor,
        "count": place.count, "from": place.from});
    replace(&places.watched(&trigger.name), &kept.to_string())
}

/// Where the watcher is for this trigger; none for a trigger it has not
/// started on, or an earlier one's of the same name. One it cannot read,
/// whole and as `save` writes it, is an error: starting over would skip
/// every turn end since.
pub(super) fn read_watched(places: &Places, trigger: &Trigger) -> Result<Option<Place>, String> {
    let path = places.watched(&trigger.name);
    if let Err(e) = std::fs::symlink_metadata(&path)
        && e.kind() == std::io::ErrorKind::NotFound
    {
        return Ok(None);
    }
    let text =
        read_record(&path).map_err(|e| format!("watch_unreadable: {}: {e}", path.display()))?;
    let unreadable = || format!("watch_unreadable: {}: not a watcher place", path.display());
    let kept: Value = serde_json::from_str(&text).map_err(|_| unreadable())?;
    if kept["generation"].as_str().ok_or_else(unreadable)? != trigger.generation {
        return Ok(None);
    }
    match (
        kept["cursor"].as_i64(),
        kept["count"].as_u64(),
        kept["from"].as_i64(),
    ) {
        (Some(cursor), Some(count), Some(from)) if from <= cursor => Ok(Some(Place {
            cursor,
            count,
            from,
        })),
        _ => Err(unreadable()),
    }
}

/// An agent's newest event cursor. Its events after a cursor are none from
/// that cursor on, so bisecting finds it in about 2·log2(cursor) indexed
/// lookups, with no daemon change.
pub(super) async fn newest_cursor(client: &Client, bot: &str) -> Result<i64, String> {
    let none_after = |after: i64| async move {
        client
            .request("events", json!({"bot": bot, "after": after, "limit": 1}))
            .await
            .map(|page| page["events"].as_array().is_none_or(Vec::is_empty))
            .map_err(super::coded)
    };
    let mut high = 1i64;
    while !none_after(high).await? {
        high = high.saturating_mul(2);
    }
    let mut low = 0;
    while low < high {
        let mid = low + (high - low) / 2;
        if none_after(mid).await? {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    Ok(high)
}

/// The watcher's LaunchAgent: started at login and whenever it stops
/// other than by finding nothing to watch.
fn plist(app: &Path, environment: &[(&str, String)]) -> String {
    let string = |s: &str| format!("<string>{}</string>", escape(s));
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    out += &format!("  <key>Label</key>\n  {}\n", string(WATCH_LABEL));
    out += &format!(
        "  <key>ProgramArguments</key>\n  <array>\n    {}\n    {}\n  </array>\n",
        string(&app.to_string_lossy()),
        string(WATCH_FLAG)
    );
    out += "  <key>RunAtLoad</key>\n  <true/>\n";
    out += "  <key>KeepAlive</key>\n  <dict><key>SuccessfulExit</key><false/></dict>\n";
    // The fires it started outlive it: a restart is not their end.
    out += "  <key>AbandonProcessGroup</key>\n  <true/>\n";
    if !environment.is_empty() {
        out += "  <key>EnvironmentVariables</key>\n  <dict>\n";
        for (key, value) in environment {
            out += &format!("    <key>{key}</key>{}\n", string(value));
        }
        out += "  </dict>\n";
    }
    out += "  <key>ProcessType</key>\n  <string>Background</string>\n</dict>\n</plist>\n";
    out
}

/// The watcher's job, as the turn-end triggers now are: written and
/// (re)started while any is, so it reads them again, and gone with the
/// last. Without `restart` only a job whose plist changed (the app moved)
/// is loaded again.
pub(super) fn rewatch(
    places: &Places,
    app: &Path,
    environment: &[(&str, String)],
    restart: bool,
    launchd: Loader,
) -> Result<(), String> {
    let path = places.watcher();
    let text = plist(app, environment);
    let old = std::fs::read_to_string(&path).ok();
    if any_watched(places) && old.as_deref() != Some(&text) {
        // One that will not load gives way to the one that ran.
        return swap(&path, WATCH_LABEL, old.as_deref(), &text, launchd);
    }
    match restart || !any_watched(places) {
        true => unwatch(places, launchd),
        false => Ok(()),
    }
}

/// The watcher reads the turn-end triggers there are again, or goes with
/// the last. Its plist stays as the app wrote it.
pub(super) fn unwatch(places: &Places, launchd: Loader) -> Result<(), String> {
    let path = places.watcher();
    if !path.exists() {
        return Ok(());
    }
    if !any_watched(places) {
        unload(WATCH_LABEL, launchd)?;
        return forget(&path);
    }
    launchd(Launchd::Restart(WATCH_LABEL)).or_else(|_| launchd(Launchd::Load(&path)))
}

fn any_watched(places: &Places) -> bool {
    read_all(places).any(|(_, read)| read.is_ok_and(|(t, _)| t.turn_end.is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watched(name: &str, source: &str, count: Option<u64>, cursor: i64) -> Watched {
        let trigger = Trigger {
            name: name.into(),
            generation: "g".into(),
            not_before: None,
            target: Target::Bot {
                name: "p.lead".into(),
                id: 1,
            },
            when: match count {
                Some(n) => format!("every {n} turns of {source}"),
                None => format!("turn end of {source}"),
            },
            at: None,
            commit: None,
            dir: None,
            reply_to: None,
            gate: None,
            runs: None,
            turn_end: Some((source.into(), 5)),
            count,
            file: None,
            daemon: Daemon {
                store: None,
                socket: Some("/tmp/s".into()),
            },
            store_id: "00ab".into(),
            message: "look".into(),
        };
        Watched {
            trigger,
            cursor: Some(cursor),
            from: None,
            count: 0,
            done: false,
        }
    }

    fn ended(bot: &str, turn: i64, cursor: i64) -> Value {
        json!({"bot": bot, "event": "turn_finished", "turn": turn, "cursor": cursor,
            "data": {"status": "completed"}})
    }

    fn accepted(bot: &str, turn: i64, cursor: i64, request_id: &str) -> Value {
        json!({"bot": bot, "event": "accepted", "turn": turn, "cursor": cursor,
            "data": {"request_id": request_id}})
    }

    #[test]
    fn a_turn_end_fires_once_and_saying_which_turn() {
        let mut w = vec![
            watched("t", "p.task", None, 10),
            watched("u", "p.other", None, 0),
        ];
        let mut sent = Sent::new();
        // Before its cursor: already counted, as on a replay after a restart.
        assert_eq!(step(&mut w, &mut sent, &ended("p.task", 3, 10)), vec![]);
        assert_eq!(
            step(&mut w, &mut sent, &ended("p.task", 4, 14)),
            vec![Step::Fire(
                0,
                "turn end of p.task: turn:p.task/4 completed".into()
            )]
        );
        assert_eq!(w[0].cursor, Some(14));
        // The same event again, from a second follow, is not another.
        assert_eq!(step(&mut w, &mut sent, &ended("p.task", 4, 14)), vec![]);
        // Other agents' events and other kinds pass by.
        assert_eq!(
            step(
                &mut w,
                &mut sent,
                &json!({"bot": "p.task", "event": "message", "turn": 5, "cursor": 15})
            ),
            vec![]
        );
        assert_eq!(w[1].cursor, Some(0));
    }

    #[test]
    fn a_count_fires_on_every_nth_turn_end() {
        let mut w = vec![watched("t", "Home", Some(3), 0)];
        let mut sent = Sent::new();
        let fired: Vec<_> = (1..=7)
            .map(|turn| step(&mut w, &mut sent, &ended("Home", turn, turn * 10)))
            .collect();
        assert_eq!(fired[0], vec![Step::Save(0)]);
        assert_eq!(fired[1], vec![Step::Save(0)]);
        assert_eq!(
            fired[2],
            vec![Step::Fire(
                0,
                "every 3 turns of Home: turn:Home/3 completed".into()
            )]
        );
        assert!(matches!(fired[5][0], Step::Fire(..)));
        assert_eq!(fired[6], vec![Step::Save(0)]);
        assert_eq!(w[0].count, 1);
    }

    #[test]
    fn a_turn_its_own_trigger_sent_does_not_wake_it() {
        let mut w = vec![
            watched("t", "p.lead", None, 0),
            watched("u", "p.lead", None, 0),
        ];
        w[1].trigger.generation = "1-23".into();
        w[0].trigger.generation = "1-2".into();
        let mut sent = Sent::new();
        // An answer `t` queued back to the agent it watches.
        step(
            &mut w,
            &mut sent,
            &accepted("p.lead", 8, 20, "trigger_1-2_reply.5.3"),
        );
        assert_eq!(
            step(&mut w, &mut sent, &ended("p.lead", 8, 22)),
            vec![
                Step::Save(0),
                Step::Fire(1, "turn end of p.lead: turn:p.lead/8 completed".into())
            ]
        );
        // Generation `1-23` is another trigger's, not `t`'s.
        step(
            &mut w,
            &mut sent,
            &accepted("p.lead", 9, 23, "trigger_1-23_5.1.2"),
        );
        assert!(matches!(
            step(&mut w, &mut sent, &ended("p.lead", 9, 25))[0],
            Step::Fire(0, _)
        ));
        assert!(sent.is_empty());
    }

    #[test]
    fn its_agent_replaced_under_its_name_ends_it() {
        let mut w = vec![watched("t", "p.task", None, 0)];
        let mut sent = Sent::new();
        let created = |id: i64| json!({"bot": "p.task", "event": "created", "cursor": 30, "data": {"id": id}});
        assert_eq!(step(&mut w, &mut sent, &created(5)), vec![]);
        assert_eq!(
            step(&mut w, &mut sent, &created(6)),
            vec![Step::Gone(
                0,
                "bot_not_found: p.task is another agent now".into()
            )]
        );
        assert!(w[0].done);
        assert_eq!(step(&mut w, &mut sent, &ended("p.task", 1, 31)), vec![]);
    }

    #[test]
    fn its_agent_deleted_while_followed_ends_it() {
        let mut w = vec![
            watched("t", "p.task", None, 0),
            watched("u", "p.other", None, 0),
        ];
        let mut sent = Sent::new();
        let deleted = json!({"bot": "p.task", "event": "deleted", "durable": false});
        assert_eq!(
            step(&mut w, &mut sent, &deleted),
            vec![Step::Gone(0, "bot_not_found: p.task was deleted".into())]
        );
        assert!(w[0].done && !w[1].done);
    }

    #[test]
    fn events_pruned_before_it_read_them_are_said_not_counted() {
        let mut w = vec![
            watched("t", "p.task", None, 10),
            watched("u", "p.task", None, 40),
            // `before` is the last event removed: one short of it missed it.
            watched("v", "p.task", None, 29),
            watched("x", "p.task", None, 30),
            // Read again from before a turn it sent: that turn's start is gone.
            watched("y", "p.task", None, 40),
            watched("z", "p.task", None, 40),
        ];
        (w[4].from, w[5].from) = (Some(20), Some(30));
        let mut sent = Sent::new();
        let pruned = json!({"bot": "p.task", "event": "pruned", "before": 30, "durable": false});
        let steps = step(&mut w, &mut sent, &pruned);
        assert!(matches!(
            &steps[..],
            [Step::Gap(0, why), Step::Gap(2, _), Step::Gap(4, _)] if why.starts_with("events_pruned:")
        ));
        // What is gone is passed over, never counted after.
        let cursors: Vec<_> = w.iter().map(|w| w.cursor).collect();
        assert_eq!(
            cursors,
            [30, 40, 30, 30, 40, 40].map(Some),
            "cursors past what is gone"
        );
    }

    #[test]
    fn a_gap_is_said_once_keeping_a_message_begun_and_its_trigger_goes_on() {
        let root = std::env::temp_dir().join(format!("agent-watch-gap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places {
            agents: root.join("LaunchAgents"),
            state: root.join("triggers"),
        };
        std::fs::create_dir_all(&places.agents).unwrap();
        let mut w = vec![watched("t", "p.task", None, 10)];
        let when = every("30m", 0).unwrap();
        let text = super::super::plist(
            Path::new("/A/agent-app"),
            &w[0].trigger,
            &when,
            &places.asks("t"),
            &[],
        );
        std::fs::write(places.plist("t"), text).unwrap();
        // A fire cut short with its message begun.
        let sending = json!({"bot": "p.lead", "request_id": "trigger_g_1"});
        keep(&places, &w[0].trigger, |k| k.sending = sending.clone()).unwrap();
        let mut sent = Sent::new();
        let pruned = |before: i64| json!({"bot": "p.task", "event": "pruned", "before": before});
        let steps = step(&mut w, &mut sent, &pruned(30));
        let [gap] = &steps[..] else {
            panic!("{steps:?}")
        };
        let Step::Gap(0, why) = gap else {
            panic!("{gap:?}")
        };
        assert!(act(&places, &mut w, &sent, Step::Gap(0, why.clone())));
        // It says so and goes on, from past what is gone, the message
        // begun still there to finish.
        let kept = state(&places, &w[0].trigger);
        assert_eq!(
            (&kept["last"]["outcome"], &kept["last"]["detail"]),
            (&json!("failed"), &json!(why))
        );
        assert_eq!(kept["sending"], sending);
        assert!(!w[0].done);
        assert_eq!(
            read_watched(&places, &w[0].trigger),
            Ok(Some(Place {
                cursor: 30,
                count: 0,
                from: 30
            }))
        );
        // Read again from there, the same gap is not said again; turn ends
        // after it count.
        assert_eq!(step(&mut w, &mut sent, &pruned(30)), vec![]);
        assert!(matches!(
            step(&mut w, &mut sent, &ended("p.task", 9, 31))[..],
            [Step::Fire(0, _)]
        ));
        // Its trigger gone: done, with nothing said.
        std::fs::remove_file(places.plist("t")).unwrap();
        assert!(act(&places, &mut w, &sent, Step::Gap(0, "x".into())));
        assert!(w[0].done);
        assert_eq!(state(&places, &w[0].trigger)["last"]["detail"], json!(why));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_place_is_read_whole_or_not_at_all() {
        let root = std::env::temp_dir().join(format!("agent-watch-place-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places {
            agents: root.join("LaunchAgents"),
            state: root.join("triggers"),
        };
        std::fs::create_dir_all(&places.state).unwrap();
        let w = watched("t", "p.task", None, 0);
        let read = |text: &str| {
            std::fs::write(places.watched("t"), text).unwrap();
            read_watched(&places, &w.trigger)
        };
        assert_eq!(
            read(r#"{"generation": "g", "cursor": 9, "count": 1, "from": 4}"#),
            Ok(Some(Place {
                cursor: 9,
                count: 1,
                from: 4
            }))
        );
        // An earlier trigger's of the same name is none.
        assert_eq!(
            read(r#"{"generation": "f", "cursor": 9, "count": 1, "from": 4}"#),
            Ok(None)
        );
        // Any part missing, of another kind, or a place it never writes.
        for text in [
            r#"{"cursor": 9, "count": 1, "from": 4}"#,
            r#"{"generation": 1, "cursor": 9, "count": 1, "from": 4}"#,
            r#"{"generation": "g", "count": 1, "from": 4}"#,
            r#"{"generation": "g", "cursor": 9, "from": 4}"#,
            r#"{"generation": "g", "cursor": 9, "count": -1, "from": 4}"#,
            r#"{"generation": "g", "cursor": 9, "count": 1}"#,
            r#"{"generation": "g", "cursor": 9, "count": 1, "from": "4"}"#,
            r#"{"generation": "g", "cursor": 9, "count": 1, "from": 10}"#,
            "[]",
        ] {
            assert!(
                read(text).is_err_and(|e| e.starts_with("watch_unreadable")),
                "{text}"
            );
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_counted_turn_end_is_an_ask_saying_why_and_only_then_saved() {
        let root = std::env::temp_dir().join(format!("agent-watch-ask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places {
            agents: root.join("LaunchAgents"),
            state: root.join("triggers"),
        };
        std::fs::create_dir_all(&places.agents).unwrap();
        let mut w = vec![watched("t", "p.task", None, 7)];
        let when = every("30m", 0).unwrap();
        let text = super::super::plist(
            Path::new("/A/agent-app"),
            &w[0].trigger,
            &when,
            &places.asks("t"),
            &[],
        );
        std::fs::write(places.plist("t"), text).unwrap();
        let why = "turn end of p.task: turn:p.task/3 completed";
        assert!(act(
            &places,
            &mut w,
            &Sent::new(),
            Step::Fire(0, why.into())
        ));
        assert_eq!(
            read_watched(&places, &w[0].trigger),
            Ok(Some(Place {
                cursor: 7,
                count: 0,
                from: 7
            }))
        );
        let asked = take_ask(&places, &w[0].trigger).unwrap().unwrap();
        assert_eq!((asked.why.as_str(), asked.kind), (why, Ask::Turn(7)));
        // A watcher stopped before it saved asks again: the same ask, which
        // the fire holding it takes with it.
        assert!(act(
            &places,
            &mut w,
            &Sent::new(),
            Step::Fire(0, why.into())
        ));
        assert_eq!(std::fs::read_dir(places.asks("t")).unwrap().count(), 1);
        let again = take_ask(&places, &w[0].trigger).unwrap().unwrap();
        assert_eq!(again.path, asked.path);
        assert_eq!(std::fs::read_dir(places.asks("t")).unwrap().count(), 0);
        std::fs::remove_file(&asked.path).unwrap();
        // An ask it cannot make leaves its place unsaved, to read that turn
        // end again.
        std::fs::remove_file(places.watched("t")).unwrap();
        std::fs::remove_dir_all(places.asks("t")).unwrap();
        std::fs::write(places.asks("t"), "").unwrap();
        assert!(!act(
            &places,
            &mut w,
            &Sent::new(),
            Step::Fire(0, why.into())
        ));
        assert_eq!(read_watched(&places, &w[0].trigger), Ok(None));
        std::fs::remove_file(places.asks("t")).unwrap();
        // An end it cannot record leaves it watching, to end it again.
        std::fs::create_dir_all(places.last("t")).unwrap();
        w[0].done = true;
        let gone = Step::Gone(0, "bot_not_found: p.task was deleted".into());
        assert!(!act(&places, &mut w, &Sent::new(), gone));
        assert!(!w[0].done);
        std::fs::remove_dir_all(places.last("t")).unwrap();
        // Its trigger gone: done, with nothing asked.
        std::fs::remove_file(places.plist("t")).unwrap();
        assert!(act(
            &places,
            &mut w,
            &Sent::new(),
            Step::Fire(0, why.into())
        ));
        assert!(w[0].done);
        assert!(take_ask(&places, &w[0].trigger).unwrap().is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_restart_reads_again_from_before_a_turn_it_sent_that_has_not_ended() {
        let root = std::env::temp_dir().join(format!("agent-watch-from-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places {
            agents: root.join("LaunchAgents"),
            state: root.join("triggers"),
        };
        std::fs::create_dir_all(&places.agents).unwrap();
        let mut w = vec![watched("t", "p.task", None, 0)];
        let when = every("30m", 0).unwrap();
        let text = super::super::plist(
            Path::new("/A/agent-app"),
            &w[0].trigger,
            &when,
            &places.asks("t"),
            &[],
        );
        std::fs::write(places.plist("t"), text).unwrap();
        let own = sent_by(&w[0].trigger);
        let mut sent = Sent::new();
        let mut queued = accepted("p.task", 4, 5, &format!("{own}q"));
        queued["event"] = json!("queued");
        step(&mut w, &mut sent, &queued);
        // Another turn ends meanwhile; its place is past that queued turn.
        let steps = step(&mut w, &mut sent, &ended("p.task", 3, 8));
        assert!(matches!(steps[..], [Step::Fire(0, _)]));
        assert!(save(&places, &mut w[0], &sent).is_ok());
        let place = read_watched(&places, &w[0].trigger).unwrap().unwrap();
        assert_eq!((place.cursor, place.from), (8, 4));
        // Restarted, it reads from there: the queued turn is known again,
        // the turn end it counted is not counted again, and the queued
        // turn's end is the trigger's own.
        let mut again = vec![watched("t", "p.task", None, place.cursor)];
        let mut sent = Sent::new();
        step(&mut again, &mut sent, &queued);
        assert_eq!(step(&mut again, &mut sent, &ended("p.task", 3, 8)), vec![]);
        assert_eq!(
            step(&mut again, &mut sent, &ended("p.task", 4, 9)),
            vec![Step::Save(0)]
        );
        // Once it ended, the place to read again from is the cursor.
        assert!(save(&places, &mut again[0], &sent).is_ok());
        let place = read_watched(&places, &again[0].trigger).unwrap().unwrap();
        assert_eq!((place.cursor, place.from), (9, 9));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn its_own_turns_stay_its_own_however_many_turns_run() {
        let mut w = vec![watched("t", "p.task", None, 0)];
        let mut sent = Sent::new();
        let own = sent_by(&w[0].trigger);
        step(
            &mut w,
            &mut sent,
            &accepted("p.task", 1, 1, &format!("{own}x")),
        );
        // Many other agents' turns, whose ends were lost, come and go.
        for turn in 2..(MAX_SENT as i64 * 3) {
            step(
                &mut w,
                &mut sent,
                &accepted("p.other", turn, turn, "trigger_other_x"),
            );
            assert!(sent.len() <= MAX_SENT);
        }
        step(
            &mut w,
            &mut sent,
            &accepted("p.task", 5000, 5000, &format!("{own}y")),
        );
        assert_eq!(
            step(&mut w, &mut sent, &ended("p.task", 5000, 5001)),
            vec![Step::Save(0)]
        );
        // One that only queued, then ended without starting, is its own too.
        let mut queued = accepted("p.task", 6000, 6000, &format!("{own}z"));
        queued["event"] = json!("queued");
        step(&mut w, &mut sent, &queued);
        assert_eq!(
            step(&mut w, &mut sent, &ended("p.task", 6000, 6001)),
            vec![Step::Save(0)]
        );
    }

    #[test]
    fn the_watcher_is_there_while_a_turn_end_trigger_is() {
        let root = std::env::temp_dir().join(format!("agent-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let places = Places {
            agents: root.join("LaunchAgents"),
            state: root.join("triggers"),
        };
        std::fs::create_dir_all(&places.agents).unwrap();
        let calls = std::cell::RefCell::new(Vec::new());
        let launchd = |what: Launchd| {
            calls.borrow_mut().push(match what {
                Launchd::Load(path) => {
                    format!("load {}", path.file_name().unwrap().to_string_lossy())
                }
                Launchd::Unload(label) => format!("unload {label}"),
                Launchd::Restart(label) => format!("restart {label}"),
            });
            Ok(())
        };
        let app = Path::new("/A/app");
        let env = [("HOME", "/Users/a".to_owned())];
        // None yet: nothing to do.
        rewatch(&places, app, &env, true, &launchd).unwrap();
        assert!(calls.borrow().is_empty() && !places.watcher().exists());
        let w = watched("t", "p.task", None, 0);
        std::fs::write(
            places.plist("t"),
            super::super::plist(
                app,
                &w.trigger,
                &When {
                    text: String::new(),
                    entries: Vec::new(),
                    not_before: None,
                    at: None,
                    watch: None,
                },
                &places.asks("t"),
                &[],
            ),
        )
        .unwrap();
        // The first: written and loaded.
        rewatch(&places, app, &env, true, &launchd).unwrap();
        let text = std::fs::read_to_string(places.watcher()).unwrap();
        assert!(
            text.contains("<string>--trigger-watch</string>")
                && text.contains("<key>SuccessfulExit</key><false/>")
        );
        assert_eq!(
            calls.take(),
            vec![
                format!("unload {WATCH_LABEL}"),
                format!("load {WATCH_LABEL}.plist")
            ]
        );
        // Another: restarted, to read it. At the app's start, left alone.
        rewatch(&places, app, &env, true, &launchd).unwrap();
        assert_eq!(calls.take(), vec![format!("restart {WATCH_LABEL}")]);
        rewatch(&places, app, &env, false, &launchd).unwrap();
        assert!(calls.borrow().is_empty());
        // The app moved: its plist follows.
        rewatch(&places, Path::new("/B/app"), &env, false, &launchd).unwrap();
        assert!(
            std::fs::read_to_string(places.watcher())
                .unwrap()
                .contains("/B/app")
        );
        calls.take();
        // A replacement that will not load gives way to the one that ran.
        let refusing = |what: Launchd| match what {
            Launchd::Load(path) if std::fs::read_to_string(path).unwrap().contains("/C/app") => {
                Err("launchctl bootstrap: refused".to_owned())
            }
            other => launchd(other),
        };
        assert!(rewatch(&places, Path::new("/C/app"), &env, false, &refusing).is_err());
        assert!(
            std::fs::read_to_string(places.watcher())
                .unwrap()
                .contains("/B/app")
        );
        assert_eq!(
            calls.take(),
            vec![
                format!("unload {WATCH_LABEL}"),
                format!("load {WATCH_LABEL}.plist")
            ]
        );
        // The last going, its plist unreadable but its place kept: its own
        // job goes first, then the watcher, which may be what retires it.
        let place = |cursor, count| Place {
            cursor,
            count,
            from: cursor,
        };
        save_watched(&places, &w.trigger, place(3, 0)).unwrap();
        std::fs::write(places.plist("t"), "not a plist").unwrap();
        retire(&places, "t", false, &launchd).unwrap();
        assert_eq!(
            calls.take(),
            vec![format!("unload {LABEL}t"), format!("unload {WATCH_LABEL}")]
        );
        assert!(!places.watcher().exists() && !places.watched("t").exists());
        // A kept place is this generation's only.
        save_watched(&places, &w.trigger, place(41, 2)).unwrap();
        assert_eq!(read_watched(&places, &w.trigger), Ok(Some(place(41, 2))));
        let later = Trigger {
            generation: "h".into(),
            ..w.trigger.clone()
        };
        assert_eq!(read_watched(&places, &later), Ok(None));
        // One it cannot read is no fresh start: that would skip turn ends.
        std::fs::write(places.watched("t"), "{\"generation\": \"g\"").unwrap();
        assert!(
            read_watched(&places, &w.trigger)
                .unwrap_err()
                .starts_with("watch_unreadable")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
