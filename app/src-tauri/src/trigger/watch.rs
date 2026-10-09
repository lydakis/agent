//! The turn-end watcher: one process, while any trigger waits on an agent's
//! turn ends, for all of them. It connects once per daemon and follows each
//! of those agents' events; between turn ends it waits on the socket, and
//! with no daemon there it tries again, at most once a minute. A turn end
//! that counts starts the trigger's own fire, the one launchd would run,
//! told why by the `fire` note. Where it is in each agent's events is kept
//! per trigger (`NAME.watch`), so a restart neither misses nor repeats one.
use super::*;
use std::collections::HashMap;

const MAX_WAIT: Duration = Duration::from_secs(60);
/// Turns a trigger sent that have not ended yet; more is a leak, not work.
const MAX_SENT: usize = 1024;

/// One turn-end trigger, as the watcher holds it.
struct Watched {
    trigger: Trigger,
    app: PathBuf,
    /// The last event of its agent it has counted, or passed over.
    cursor: Option<i64>,
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
    /// Retention removed events it had not read: turn ends went uncounted,
    /// which its last result says.
    Gap(usize, String),
}

/// The `request_id`s of turns triggers sent, by agent and turn, from their
/// `accepted` events, until those turns end.
type Sent = HashMap<(String, i64), String>;

/// What one event means for the triggers following its agent. Only a turn
/// that ends after a trigger's cursor counts, and not one the trigger sent.
fn step(watched: &mut [Watched], sent: &mut Sent, event: &Value) -> Vec<Step> {
    let (Some(bot), Some(kind)) = (event["bot"].as_str(), event["event"].as_str()) else {
        return Vec::new();
    };
    let mut steps = Vec::new();
    match kind {
        "accepted" => {
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
                sent.insert((bot.to_owned(), turn), id.to_owned());
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
        "pruned" => {
            let before = event["before"].as_i64().unwrap_or(0);
            for (i, w) in watched.iter().enumerate() {
                if !w.done && w.source() == bot && w.cursor.is_none_or(|c| c + 1 < before) {
                    steps.push(Step::Gap(i, format!(
                        "events_pruned: turn ends of {bot} before event {before} were removed before they were read, and not counted"
                    )));
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
            let by = sent.remove(&(bot.to_owned(), turn));
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
    for (trigger, app) in read_all(&places).filter_map(|(_, read)| read.ok()) {
        if trigger.turn_end.is_none() {
            continue;
        }
        let Ok(socket) = trigger.daemon.socket() else {
            continue;
        };
        let (cursor, count) = match read_watched(&places, &trigger) {
            Some((cursor, count)) => (Some(cursor), count),
            None => (None, 0),
        };
        daemons
            .entry((socket, trigger.store_id.clone()))
            .or_default()
            .push(Watched {
                trigger,
                app,
                cursor,
                count,
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
    let mut fires = Fires::new();
    // A fire that ended its own trigger (its runs, its agent gone) says so
    // here when it exits, so nothing is followed for it after.
    let (exited, mut exits) = tokio::sync::mpsc::unbounded_channel::<String>();
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
        if let Err(error) = begin(&places, &client, &mut watched).await {
            log(error);
            client.close().await;
            tokio::time::sleep(MAX_WAIT).await;
            continue;
        }
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else { break };
                    for step in step(&mut watched, &mut sent, &event) {
                        act(&places, &mut watched, &mut fires, &exited, step);
                    }
                }
                Some(name) = exits.recv() => {
                    for w in watched.iter_mut().filter(|w| !w.done && w.trigger.name == name) {
                        let there = read_trigger(&places.plist(&name)).ok().map(|(t, _)| t);
                        w.done = there.as_ref() != Some(&w.trigger);
                    }
                }
            }
            if watched.iter().all(|w| w.done) {
                break;
            }
        }
        client.close().await;
    }
}

/// On each connection: end the triggers whose agent is gone, find where
/// a new one starts, and follow each agent from the earliest place any of
/// its triggers is at.
async fn begin(places: &Places, client: &Client, watched: &mut [Watched]) -> Result<(), String> {
    for w in watched.iter_mut().filter(|w| !w.done) {
        let now = match bot_id(client, w.source()).await {
            Ok(id) => Some(id),
            Err(error) if error.starts_with("bot_not_found") => None,
            Err(error) => return Err(error),
        };
        if now != w.source_id() {
            w.done = true;
            let gone = json!({"outcome": "gone", "detail": format!("bot_not_found: {} is gone", w.source())});
            settle(
                places,
                &w.trigger,
                &gone,
                &Kept::of(&state(places, &w.trigger)),
                &launchctl,
            );
            continue;
        }
        if w.cursor.is_none() {
            w.cursor = Some(newest_cursor(client, w.source()).await?);
            save(places, w);
        }
    }
    let mut from = HashMap::<&str, i64>::new();
    for w in watched.iter().filter(|w| !w.done) {
        let at = w.cursor.unwrap_or(0);
        from.entry(w.source())
            .and_modify(|c| *c = (*c).min(at))
            .or_insert(at);
    }
    for (bot, after) in from {
        client
            .request("follow", json!({"bot": bot, "after": after}))
            .await
            .map_err(super::coded)?;
    }
    Ok(())
}

/// The fire each trigger last started, until it exits.
type Fires = HashMap<String, tokio::task::JoinHandle<()>>;

fn act(
    places: &Places,
    watched: &mut [Watched],
    fires: &mut Fires,
    exited: &tokio::sync::mpsc::UnboundedSender<String>,
    step: Step,
) {
    let log = |error: String| eprintln!("{}", error_json(&error));
    match step {
        Step::Save(i) => save(places, &watched[i]),
        Step::Gone(i, detail) => {
            let trigger = &watched[i].trigger;
            let gone = json!({"outcome": "gone", "detail": detail});
            settle(
                places,
                trigger,
                &gone,
                &Kept::of(&state(places, trigger)),
                &launchctl,
            );
        }
        Step::Gap(i, detail) => {
            let trigger = &watched[i].trigger;
            let gap = json!({"outcome": "failed", "detail": detail});
            settle(
                places,
                trigger,
                &gap,
                &Kept::of(&state(places, trigger)),
                &launchctl,
            );
        }
        Step::Fire(i, why) => {
            let w = &mut watched[i];
            let name = w.trigger.name.clone();
            // Ended, by its runs or `rm`, or replaced: no longer this trigger.
            let there = read_trigger(&places.plist(&name)).ok().map(|(t, _)| t);
            if there.as_ref() != Some(&w.trigger) {
                w.done = true;
                return;
            }
            // Where it is goes on disk only once the fire is handed over: a
            // watcher stopped before then reads this turn end again.
            if let Err(error) = replace(&places.asked(&name), &format!("{}\n{why}", now())) {
                return log(error);
            }
            // Its last fire still runs, as on a long `--reply-to` wait: it
            // finds the note when it is done, or the fire waiting on it does.
            if fires.get(&name).is_some_and(|fire| !fire.is_finished()) {
                return save(places, w);
            }
            // Its own process group: restarting the watcher leaves it running.
            let spawned = tokio::process::Command::new(&w.app)
                .args(w.trigger.args())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .process_group(0)
                .spawn();
            match spawned {
                Ok(mut child) => {
                    save(places, w);
                    let exited = exited.clone();
                    fires.insert(
                        name.clone(),
                        tokio::spawn(async move {
                            let _ = child.wait().await;
                            let _ = exited.send(name);
                        }),
                    );
                }
                Err(error) => log(format!("{}: {error}", w.app.display())),
            }
        }
    }
}

fn save(places: &Places, w: &Watched) {
    if let Err(error) = save_watched(places, &w.trigger, w.cursor.unwrap_or(0), w.count) {
        eprintln!("{}", error_json(&error));
    }
}

pub(super) fn save_watched(
    places: &Places,
    trigger: &Trigger,
    cursor: i64,
    count: u64,
) -> Result<(), String> {
    let kept = json!({"generation": trigger.generation, "cursor": cursor, "count": count});
    replace(&places.watched(&trigger.name), &kept.to_string())
}

/// Where the watcher is for this trigger, when it is this trigger's.
pub(super) fn read_watched(places: &Places, trigger: &Trigger) -> Option<(i64, u64)> {
    let kept = read_json(&places.watched(&trigger.name))?;
    (kept["generation"] == trigger.generation)
        .then(|| {
            Some((
                kept["cursor"].as_i64()?,
                kept["count"].as_u64().unwrap_or(0),
            ))
        })
        .flatten()
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
    let any = read_all(places).any(|(_, read)| read.is_ok_and(|(t, _)| t.turn_end.is_some()));
    if !any {
        if !path.exists() {
            return Ok(());
        }
        unload(WATCH_LABEL, launchd)?;
        return forget(&path);
    }
    let text = plist(app, environment);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(text.as_str()) {
        unload(WATCH_LABEL, launchd)?;
        replace(&path, &text)?;
        return launchd(Launchd::Load(&path));
    }
    if !restart {
        return Ok(());
    }
    launchd(Launchd::Restart(WATCH_LABEL)).or_else(|_| launchd(Launchd::Load(&path)))
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
            app: "/A/app".into(),
            cursor: Some(cursor),
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
        ];
        let mut sent = Sent::new();
        let pruned = json!({"bot": "p.task", "event": "pruned", "before": 30, "durable": false});
        let steps = step(&mut w, &mut sent, &pruned);
        assert!(matches!(&steps[..], [Step::Gap(0, why)] if why.starts_with("events_pruned:")));
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
                Launchd::Start(label) => format!("start {label}"),
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
        // The last gone: so is the watcher.
        std::fs::remove_file(places.plist("t")).unwrap();
        rewatch(&places, app, &env, true, &launchd).unwrap();
        assert_eq!(calls.take(), vec![format!("unload {WATCH_LABEL}")]);
        assert!(!places.watcher().exists());
        // A kept place is this generation's only.
        save_watched(&places, &w.trigger, 41, 2).unwrap();
        assert_eq!(read_watched(&places, &w.trigger), Some((41, 2)));
        let later = Trigger {
            generation: "h".into(),
            ..w.trigger.clone()
        };
        assert_eq!(read_watched(&places, &later), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
