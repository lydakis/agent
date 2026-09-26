//! Live event fan-out. The stdio owner receives everything with backpressure;
//! socket followers receive their bots' events and are dropped when they lag.
//! A follower of `*` receives every bot's events on one connection, replayed
//! from a store-wide cursor, so a fleet controller needs one subscription.
use agent_runtime::{Result, fail, output::Output, store::Store};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

pub struct Subscription {
    session: u64,
    output: Output,
    live: bool,
    cancelled: bool,
    replay: Option<tokio::task::AbortHandle>,
    last_cursor: i64,
}
impl Subscription {
    pub fn set_replay(&mut self, task: tokio::task::AbortHandle) {
        if self.cancelled {
            task.abort();
        }
        self.replay = Some(task);
    }
    fn cancel(&mut self) {
        self.cancelled = true;
        if let Some(task) = self.replay.take() {
            task.abort();
        }
    }
}
pub type Sub = Arc<Mutex<Subscription>>;
/// Keep `subs` other than `session`'s, cancelling that session's. The
/// session is read without locking, since a replay holds its subscription's
/// lock across a store page.
fn retain_other_session(subs: &mut Vec<(u64, Sub)>, session: u64) {
    subs.retain(|(id, sub)| {
        if *id == session {
            sub.lock().unwrap().cancel();
        }
        *id != session
    });
}
/// The subscription name that means every bot.
pub const ALL: &str = "*";
#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
    /// The newest durable cursor delivered to every follower.
    published: Arc<watch::Sender<i64>>,
}
#[derive(Default)]
struct HubInner {
    firehose: Vec<(u64, Output)>,
    subs: HashMap<String, Vec<(u64, Sub)>>,
}
impl Hub {
    pub fn add_firehose(&self, session: u64, output: Output) {
        self.inner.lock().unwrap().firehose.push((session, output));
    }
    pub fn subscribe(&self, bot: &str, session: u64, output: Output, after: i64) -> Sub {
        let mut inner = self.inner.lock().unwrap();
        let subs = inner.subs.entry(bot.to_owned()).or_default();
        retain_other_session(subs, session);
        let sub = Arc::new(Mutex::new(Subscription {
            session,
            output,
            live: false,
            cancelled: false,
            replay: None,
            last_cursor: after,
        }));
        subs.push((session, sub.clone()));
        sub
    }
    pub fn unsubscribe(&self, bot: &str, session: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(subs) = inner.subs.get_mut(bot) {
            retain_other_session(subs, session);
            if subs.is_empty() {
                inner.subs.remove(bot);
            }
        }
    }
    pub fn close_session(&self, session: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.firehose.retain(|(id, _)| *id != session);
        inner.subs.retain(|_, subs| {
            retain_other_session(subs, session);
            !subs.is_empty()
        });
    }
    /// How many copies of one of `bot`'s events `session` receives: once
    /// for the firehose, once following `*`, and once following the bot.
    pub fn deliveries(&self, bot: &str, session: u64) -> usize {
        let inner = self.inner.lock().unwrap();
        let follows = |name: &str| {
            inner.subs.get(name).map_or(0, |subs| {
                subs.iter().filter(|(id, _)| *id == session).count()
            })
        };
        let firehose = inner
            .firehose
            .iter()
            .filter(|(id, _)| *id == session)
            .count();
        firehose + follows(ALL) + if bot == ALL { 0 } else { follows(bot) }
    }
    fn firehose(&self) -> Vec<Output> {
        self.inner
            .lock()
            .unwrap()
            .firehose
            .iter()
            .map(|(_, o)| o.clone())
            .collect()
    }
    /// Deliver to followers; `cursor` orders durable entries after replay.
    fn fan_out(&self, bot: &str, event: &Value, cursor: Option<i64>) {
        let subs: Vec<Sub> = {
            let inner = self.inner.lock().unwrap();
            let followers = |name: &str| {
                inner
                    .subs
                    .get(name)
                    .into_iter()
                    .flatten()
                    .map(|(_, sub)| sub.clone())
            };
            followers(bot).chain(followers(ALL)).collect::<Vec<Sub>>()
        };
        for sub in subs {
            let (session, output) = {
                let mut s = sub.lock().unwrap();
                if s.cancelled || !s.live || cursor.is_some_and(|c| c <= s.last_cursor) {
                    continue;
                }
                if let Some(c) = cursor {
                    s.last_cursor = c;
                }
                (s.session, s.output.clone())
            };
            if output.try_send(event.clone()).is_err() {
                self.close_session(session);
                output.close();
            }
        }
    }
    pub async fn durable(&self, bot: &str, entry: Value) -> Result<()> {
        let cursor = entry["cursor"].as_i64();
        self.fan_out(bot, &entry, cursor);
        let mut sent = Ok(());
        for output in self.firehose() {
            sent = output.send(entry.clone()).await;
            if sent.is_err() {
                break;
            }
        }
        if let Some(cursor) = cursor {
            self.published_to(cursor);
        }
        sent
    }
    /// The newest durable cursor delivered: every event up to it is in its
    /// followers' queues, refused and their sessions closed, or was removed
    /// before publication.
    pub fn published(&self) -> i64 {
        *self.published.borrow()
    }
    pub fn published_to(&self, cursor: i64) {
        self.published.send_if_modified(|published| {
            let moved = cursor > *published;
            if moved {
                *published = cursor;
            }
            moved
        });
    }
    /// Wait until every event up to `cursor` is delivered.
    pub async fn published_through(&self, cursor: i64) {
        let mut published = self.published.subscribe();
        let _ = published.wait_for(|published| *published >= cursor).await;
    }
    pub async fn live(&self, bot: &str, event: Value) -> Result<()> {
        self.fan_out(bot, &event, None);
        for output in self.firehose() {
            output.send(event.clone()).await?;
        }
        Ok(())
    }
}

/// Page durable history to a follower, then hand it to live delivery. The
/// switch happens inside a store job, so no committed event can fall between
/// the last page and the first live delivery.
pub async fn replay(store: Store, hub: Hub, bot: String, sub: Sub) -> Result<()> {
    loop {
        let (page_sub, page_bot) = (sub.clone(), bot.clone());
        let mut page = store
            .op("events_after", move |db| {
                let mut s = page_sub.lock().unwrap();
                if s.cancelled {
                    return Ok(json!({"events":[]}));
                }
                let page = if page_bot == ALL {
                    db.events_after(s.last_cursor, 256)?
                } else {
                    db.events(&page_bot, s.last_cursor, 256)?
                };
                if page["events"].as_array().is_some_and(Vec::is_empty) {
                    s.live = true;
                } else if let Some(next) = page["next_cursor"].as_i64() {
                    s.last_cursor = next;
                }
                Ok(page)
            })
            .await?;
        let Value::Array(events) = page["events"].take() else {
            return fail("invalid_event_page");
        };
        let (session, output, cursor) = {
            let s = sub.lock().unwrap();
            (s.session, s.output.clone(), s.last_cursor)
        };
        if let Some(before) = page.get("pruned_before").and_then(Value::as_i64) {
            // Retention removed events the follower asked for: say so once,
            // ahead of what remains, rather than replaying a silent gap.
            let notice = json!({"event":"pruned","bot":bot,"before":before,"durable":false});
            if output.send(notice).await.is_err() {
                hub.unsubscribe(&bot, session);
                return Ok(());
            }
        }
        if events.is_empty() {
            // Marks the replay/live boundary for the follower.
            if output
                .try_send(json!({"event":"follow_live","bot":bot,"cursor":cursor}))
                .is_err()
            {
                hub.close_session(session);
                output.close();
            }
            return Ok(());
        }
        for event in events {
            if output.send(event).await.is_err() {
                hub.unsubscribe(&bot, session);
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[tokio::test]
    async fn counting_deliveries_waits_for_no_replay() {
        let hub = Hub::default();
        let sub = hub.subscribe("B", 1, Output::writer(tokio::io::sink()), 0);
        // A replay holds its subscription's lock across a store page.
        let (locked, inside) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let replaying = std::thread::spawn(move || {
            let _page = sub.lock().unwrap();
            locked.send(()).unwrap();
            released.recv().ok();
        });
        inside.recv().unwrap();
        let (counted, count) = mpsc::channel();
        let counter = hub.clone();
        std::thread::spawn(move || {
            counted
                .send((counter.deliveries("B", 1), counter.deliveries("B", 2)))
                .unwrap()
        });
        let deliveries = count.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        replaying.join().unwrap();
        assert_eq!(deliveries, Ok((1, 0)));
    }
}
