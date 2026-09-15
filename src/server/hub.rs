//! Live event fan-out. The stdio owner receives everything with backpressure;
//! socket followers receive their bots' events and are dropped when they lag.
use agent_runtime::{Result, fail, output::Output, store::Store};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

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
    fn retain_other_session(&mut self, session: u64) -> bool {
        if self.session != session {
            return true;
        }
        self.cancelled = true;
        if let Some(task) = self.replay.take() {
            task.abort();
        }
        false
    }
}
pub type Sub = Arc<Mutex<Subscription>>;
#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
}
#[derive(Default)]
struct HubInner {
    firehose: Vec<(u64, Output)>,
    subs: HashMap<String, Vec<Sub>>,
}
impl Hub {
    pub fn add_firehose(&self, session: u64, output: Output) {
        self.inner.lock().unwrap().firehose.push((session, output));
    }
    pub fn subscribe(&self, bot: &str, session: u64, output: Output, after: i64) -> Sub {
        let mut inner = self.inner.lock().unwrap();
        let subs = inner.subs.entry(bot.to_owned()).or_default();
        subs.retain(|s| s.lock().unwrap().retain_other_session(session));
        let sub = Arc::new(Mutex::new(Subscription {
            session,
            output,
            live: false,
            cancelled: false,
            replay: None,
            last_cursor: after,
        }));
        subs.push(sub.clone());
        sub
    }
    pub fn unsubscribe(&self, bot: &str, session: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(subs) = inner.subs.get_mut(bot) {
            subs.retain(|s| s.lock().unwrap().retain_other_session(session));
            if subs.is_empty() {
                inner.subs.remove(bot);
            }
        }
    }
    pub fn close_session(&self, session: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.firehose.retain(|(id, _)| *id != session);
        inner.subs.retain(|_, subs| {
            subs.retain(|s| s.lock().unwrap().retain_other_session(session));
            !subs.is_empty()
        });
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
        let subs: Vec<Sub> = self
            .inner
            .lock()
            .unwrap()
            .subs
            .get(bot)
            .map(|s| s.to_vec())
            .unwrap_or_default();
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
        for output in self.firehose() {
            output.send(entry.clone()).await?;
        }
        Ok(())
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
            .call(move |db| {
                let mut s = page_sub.lock().unwrap();
                if s.cancelled {
                    return Ok(json!({"events":[]}));
                }
                let page = db.events(&page_bot, s.last_cursor, 256)?;
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
