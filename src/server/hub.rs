//! Live event fan-out. The stdio owner receives everything with backpressure;
//! socket followers receive their bots' events and are dropped when they lag.
//! A follower of `*` receives every bot's events on one connection, replayed
//! from a store-wide cursor, so a fleet controller needs one subscription.
//! A session serving a gate tag receives only the calls announced for it.
use agent_runtime::{
    Result, fail,
    output::Output,
    store::{Served, Store},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

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
/// The session serving one gate tag. It receives each call announced for
/// the tag past `cursor`, the point where the listing it was handed ends,
/// and keeps the tag while it renews before `deadline`.
pub struct Approver {
    session: u64,
    output: Output,
    feed: Feed,
    lease: u64,
    lease_ms: u64,
    deadline: Instant,
    /// Its key in the hub's `expiries`: no later than `deadline`, which
    /// only moves later.
    indexed: Instant,
    cursor: i64,
    live: bool,
}
impl Approver {
    /// Start delivery after the listing the serving job read: only calls
    /// announced past `cursor` are new to the approver. The lease period
    /// starts here, so time queued behind storage does not use it up.
    pub fn go_live(&mut self, cursor: i64) {
        self.cursor = cursor;
        self.live = true;
        self.deadline = Instant::now() + Duration::from_millis(self.lease_ms);
    }
}
pub type Serving = Arc<Mutex<Approver>>;
/// How many pushes may wait for one holder. A group commit can publish many
/// parts at once, each up to 256 KiB of calls; they wait here for room in
/// the session's output rather than closing it. A holder this far behind
/// has stopped reading.
const FEED_PARTS: usize = 128;
/// Whether a feed's calls still go out.
#[derive(Clone, Copy, PartialEq)]
enum Flow {
    Open,
    /// Its session is serving the tag again; the calls wait until the new
    /// lease replaces this one or the new listing fails.
    Paused,
    /// The lease ended: calls still waiting are dropped, and only the
    /// notice that says so goes out.
    Ended,
}
/// The pushes to one lease's holder, in order: its calls and the notice
/// that it lost the tag.
struct Feed {
    sender: mpsc::UnboundedSender<(Value, bool)>,
    queued: Arc<AtomicUsize>,
    flow: watch::Sender<Flow>,
}
impl Feed {
    fn new(output: Output) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel::<(Value, bool)>();
        let queued = Arc::new(AtomicUsize::new(0));
        let counted = queued.clone();
        let (flow, mut state) = watch::channel(Flow::Open);
        tokio::spawn(async move {
            while let Some((message, notice)) = receiver.recv().await {
                let sent = Self::deliver(&output, &mut state, &message, notice).await;
                counted.fetch_sub(1, Ordering::Relaxed);
                if sent.is_err() {
                    break;
                }
            }
        });
        Self {
            sender,
            queued,
            flow,
        }
    }
    /// Send one push as the flow allows. A pause or an end also stops a
    /// send still waiting for room in the output, so no call goes out
    /// under a lease that ended while it waited.
    async fn deliver(
        output: &Output,
        state: &mut watch::Receiver<Flow>,
        message: &Value,
        notice: bool,
    ) -> Result<()> {
        loop {
            let flow = *state.borrow_and_update();
            if flow == Flow::Ended && !notice {
                return Ok(());
            }
            if flow == Flow::Paused {
                // Gone while paused: nobody holds the lease any more.
                if state.changed().await.is_err() {
                    return Ok(());
                }
                continue;
            }
            tokio::select! {
                sent = output.send_ref(message) => return sent,
                changed = state.changed() => if changed.is_err() {
                    // The flow can no longer change.
                    return output.send_ref(message).await;
                },
            }
        }
    }
    /// Queue a call. False when the holder has stopped reading.
    fn push(&self, message: Value) -> bool {
        self.queued.fetch_add(1, Ordering::Relaxed) < FEED_PARTS
            && self.sender.send((message, false)).is_ok()
    }
    fn pause(&self) {
        self.flow.send_if_modified(|flow| {
            let open = *flow == Flow::Open;
            if open {
                *flow = Flow::Paused;
            }
            open
        });
    }
    fn resume(&self) {
        self.flow.send_if_modified(|flow| {
            let paused = *flow == Flow::Paused;
            if paused {
                *flow = Flow::Open;
            }
            paused
        });
    }
    /// Drop the calls still waiting, and send `notice` after them.
    fn end(&self, notice: Option<Value>) {
        self.flow.send_replace(Flow::Ended);
        if let Some(notice) = notice {
            self.queued.fetch_add(1, Ordering::Relaxed);
            let _ = self.sender.send((notice, true));
        }
    }
}
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Mutex<HubInner>>,
    /// The newest durable cursor delivered to every live follower.
    published: Arc<watch::Sender<i64>>,
}
impl Default for Hub {
    fn default() -> Self {
        // Leases start from a random point each run, so a number from
        // before a restart matches the lease its tag has after only by a
        // 2^-52 chance, whatever the clock does. Below 2^53, so a client
        // reading JSON numbers as doubles keeps them exact.
        use std::hash::{BuildHasher, Hasher};
        let seed = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
            >> 12;
        Self {
            inner: Arc::new(Mutex::new(HubInner {
                next_lease: seed,
                ..HubInner::default()
            })),
            published: Arc::default(),
        }
    }
}
#[derive(Default)]
struct HubInner {
    firehose: Vec<(u64, Output)>,
    subs: HashMap<String, Vec<(u64, Sub)>>,
    approvers: HashMap<String, Serving>,
    /// Every entry of `approvers` once, by lease and a deadline no later
    /// than its own, so leases that ran out are found without visiting the
    /// rest. Renewals move only the deadline; the key catches up when it
    /// comes due.
    expiries: BTreeMap<(Instant, u64), String>,
    next_lease: u64,
}
impl HubInner {
    fn insert(&mut self, tag: &str, serving: Serving) {
        let key = {
            let mut held = serving.lock().unwrap();
            held.indexed = held.deadline;
            (held.indexed, held.lease)
        };
        self.expiries.insert(key, tag.to_owned());
        self.approvers.insert(tag.to_owned(), serving);
    }
    fn remove(&mut self, tag: &str) -> Option<Serving> {
        let serving = self.approvers.remove(tag)?;
        let key = {
            let held = serving.lock().unwrap();
            (held.indexed, held.lease)
        };
        self.expiries.remove(&key);
        Some(serving)
    }
    /// End the leases that ran out and tell their holders. A lease not yet
    /// live has not started, so it is kept for another period.
    fn expire_due(&mut self, now: Instant) {
        while let Some(entry) = self.expiries.first_entry() {
            if entry.key().0 > now {
                break;
            }
            let ((_, lease), tag) = entry.remove_entry();
            let Some(serving) = self.approvers.get(&tag).cloned() else {
                continue;
            };
            let mut held = serving.lock().unwrap();
            if held.live && held.deadline <= now {
                held.feed.end(Some(lost(&tag, lease)));
                drop(held);
                self.approvers.remove(&tag);
            } else {
                if !held.live {
                    held.deadline = now + Duration::from_millis(held.lease_ms);
                }
                held.indexed = held.deadline;
                self.expiries.insert((held.indexed, lease), tag);
            }
        }
    }
}
/// What a serving session is told when its tag passes to another holder,
/// its lease runs out, or it answers with a lease that ended.
fn lost(tag: &str, lease: u64) -> Value {
    json!({"event":"approvals_lost","tag":tag,"lease":lease,"durable":false})
}
impl Hub {
    /// Hand `tag` to `session` under a new lease, not yet live: the job
    /// that lists the tag's waiting calls makes it live. A tag another
    /// session holds is refused until that session closes or lets its
    /// lease run out; the one it had is then told so. Returns the lease the
    /// session itself held before, which `unserve` puts back.
    pub fn serve(
        &self,
        tag: &str,
        session: u64,
        output: Output,
        lease_ms: u64,
    ) -> Result<(u64, Serving, Option<Serving>)> {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        if let Some(held) = inner.approvers.get(tag) {
            let held = held.lock().unwrap();
            if held.session != session && (!held.live || held.deadline > now) {
                return fail("approvals_served");
            }
        }
        // The session's own lease on the tag is replaced without a word,
        // its calls held back until the new listing is read; another
        // session's ran out, so it is told.
        let before = inner.remove(tag).and_then(|serving| {
            let held = serving.lock().unwrap();
            if held.session == session {
                held.feed.pause();
                drop(held);
                return Some(serving);
            }
            held.feed.end(Some(lost(tag, held.lease)));
            None
        });
        // Leases that ran out go now, whichever tag they held, so tags
        // served once and let go do not accumulate.
        inner.expire_due(now);
        inner.next_lease += 1;
        let lease = inner.next_lease;
        let approver = Arc::new(Mutex::new(Approver {
            session,
            feed: Feed::new(output.clone()),
            output,
            lease,
            lease_ms,
            deadline: now + Duration::from_millis(lease_ms),
            indexed: now,
            cursor: i64::MAX,
            live: false,
        }));
        inner.insert(tag, approver.clone());
        Ok((lease, approver, before))
    }
    /// Give up `tag` if `lease` still holds it, as when serving it failed,
    /// and put back the lease the session held `before`, whose calls go
    /// out again.
    pub fn unserve(&self, tag: &str, lease: u64, before: Option<Serving>) {
        let mut inner = self.inner.lock().unwrap();
        let holds = inner
            .approvers
            .get(tag)
            .is_some_and(|held| held.lock().unwrap().lease == lease);
        if holds && let Some(failed) = inner.remove(tag) {
            failed.lock().unwrap().feed.end(None);
        }
        match before {
            Some(before) if holds => {
                before.lock().unwrap().feed.resume();
                inner.insert(tag, before);
            }
            Some(before) => before.lock().unwrap().feed.end(None),
            None => {}
        }
    }
    /// The session's new lease on a tag holds: the calls its lease from
    /// `before` still had waiting are on the new listing, so they are
    /// dropped.
    pub fn replaced(&self, before: Option<Serving>) {
        if let Some(before) = before {
            before.lock().unwrap().feed.end(None);
        }
    }
    /// Keep `tag` for `session` under `lease` for another lease period.
    /// A lease that ended, by takeover, by running out, or by the session
    /// closing, is refused with `approvals_lost`; one that ran out is ended
    /// here, and its holder told.
    pub fn renew(&self, tag: &str, session: u64, lease: u64) -> Result<u64> {
        let mut inner = self.inner.lock().unwrap();
        let Some(held) = inner.approvers.get(tag).cloned() else {
            return fail("approvals_lost");
        };
        let mut held = held.lock().unwrap();
        if held.session != session || held.lease != lease {
            return fail("approvals_lost");
        }
        let now = Instant::now();
        if held.deadline <= now {
            held.feed.end(Some(lost(tag, lease)));
            drop(held);
            inner.remove(tag);
            return fail("approvals_lost");
        }
        held.deadline = now + Duration::from_millis(held.lease_ms);
        Ok(held.lease_ms)
    }
    /// The tags served now, for `stats`.
    pub fn served(&self) -> Vec<String> {
        let now = Instant::now();
        let inner = self.inner.lock().unwrap();
        let mut tags: Vec<String> = inner
            .approvers
            .iter()
            .filter(|(_, held)| {
                let held = held.lock().unwrap();
                held.live && held.deadline > now
            })
            .map(|(tag, _)| tag.clone())
            .collect();
        tags.sort();
        tags
    }
    /// Deliver a committed round to the sessions serving its gate tags,
    /// each only the calls waiting on its tag. A holder whose lease ran out
    /// is told it lost the tag instead; one that has stopped reading is
    /// closed, like a lagging follower.
    pub fn approval(&self, round: &Served) {
        for tag in &round.tags {
            self.deliver(tag, round);
        }
    }
    fn deliver(&self, tag: &str, round: &Served) {
        let Some(held) = self.inner.lock().unwrap().approvers.get(tag).cloned() else {
            return;
        };
        let lease = {
            let held = held.lock().unwrap();
            if !held.live || round.cursor <= held.cursor {
                return;
            }
            if held.deadline <= Instant::now() {
                let (session, lease) = (held.session, held.lease);
                drop(held);
                self.expire(tag, session, lease);
                return;
            }
            held.lease
        };
        // Built outside the lock, and only for a tag someone serves. A
        // lease that ends meanwhile drops what is pushed to it.
        let Ok(messages) = round.messages(tag) else {
            return;
        };
        let held = held.lock().unwrap();
        for mut message in messages {
            message["lease"] = json!(lease);
            message["durable"] = json!(false);
            if !held.feed.push(message) {
                let (session, output) = (held.session, held.output.clone());
                drop(held);
                self.close_session(session);
                output.close();
                return;
            }
        }
    }
    /// End a lease that ran out, if it still holds its tag, and tell its
    /// holder.
    fn expire(&self, tag: &str, session: u64, lease: u64) {
        let mut inner = self.inner.lock().unwrap();
        let Some(held) = inner.approvers.get(tag).cloned() else {
            return;
        };
        let held = held.lock().unwrap();
        if held.session == session && held.lease == lease {
            held.feed.end(Some(lost(tag, lease)));
            drop(held);
            inner.remove(tag);
        }
    }
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
        // A closed session's tags are free at once; nobody is left to tell.
        let HubInner {
            approvers,
            expiries,
            ..
        } = &mut *inner;
        approvers.retain(|_, serving| {
            let held = serving.lock().unwrap();
            if held.session == session {
                expiries.remove(&(held.indexed, held.lease));
                held.feed.end(None);
            }
            held.session != session
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
    /// Whether one of `session`'s follows that receives `bot`'s events is
    /// still paging history: the publisher passes those follows by, and
    /// their replay delivers the events when it reaches them. A follow busy
    /// with a page counts as replaying, so this never waits on a store job.
    pub fn replaying(&self, bot: &str, session: u64) -> bool {
        let inner = self.inner.lock().unwrap();
        let replaying = |name: &str| {
            inner.subs.get(name).is_some_and(|subs| {
                subs.iter().any(|(id, sub)| {
                    *id == session && sub.try_lock().map_or(true, |s| !s.live && !s.cancelled)
                })
            })
        };
        replaying(ALL) || (bot != ALL && replaying(bot))
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
    /// The newest durable cursor published: every event up to it is in its
    /// live followers' queues, refused and their sessions closed, or was
    /// removed before publication. A follow still replaying gets it from
    /// its replay (see `replaying`).
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
    /// Wait until every event up to `cursor` is published.
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

    #[tokio::test]
    async fn serving_ends_only_the_leases_that_ran_out() {
        use tokio::io::AsyncBufReadExt;
        let hub = Hub::default();
        let (writer, reader) = tokio::io::duplex(1 << 16);
        let told = Output::writer(writer);
        let quiet = || Output::writer(tokio::io::sink());
        let (ran_out, serving, _) = hub.serve("ran-out", 1, told.clone(), 1).unwrap();
        serving.lock().unwrap().go_live(0);
        // Renewed: its deadline moved past the key it was indexed under.
        let (_, renewed, _) = hub.serve("renewed", 1, quiet(), 1).unwrap();
        renewed.lock().unwrap().go_live(0);
        renewed.lock().unwrap().deadline = Instant::now() + Duration::from_secs(3600);
        // Still being listed, so its lease has not started.
        let (listing, _, _) = hub.serve("listing", 1, quiet(), 1).unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;

        let (_, _, before) = hub.serve("other", 2, quiet(), 60_000).unwrap();
        assert!(before.is_none());
        let mut line = String::new();
        let mut reader = tokio::io::BufReader::new(reader);
        tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let lost: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            (lost["event"].as_str(), lost["lease"].as_u64()),
            (Some("approvals_lost"), Some(ran_out))
        );
        {
            let inner = hub.inner.lock().unwrap();
            let mut tags: Vec<&str> = inner.approvers.keys().map(String::as_str).collect();
            tags.sort();
            assert_eq!(tags, ["listing", "other", "renewed"]);
            // One key a lease, no later than its deadline.
            assert_eq!(inner.expiries.len(), inner.approvers.len());
            for (key, tag) in &inner.expiries {
                let held = inner.approvers[tag].lock().unwrap();
                assert_eq!(*key, (held.indexed, held.lease));
                assert!(held.indexed <= held.deadline);
            }
        }
        // Another session cannot take a tag still being listed.
        assert!(hub.serve("listing", 3, quiet(), 1).is_err());
        // Serving a tag again replaces the session's own lease silently,
        // and a failed serve puts it back under its old key.
        let (again, _, before) = hub.serve("listing", 1, quiet(), 1).unwrap();
        assert_eq!(
            before.as_ref().map(|b| b.lock().unwrap().lease),
            Some(listing)
        );
        hub.unserve("listing", again, before);
        hub.close_session(1);
        let inner = hub.inner.lock().unwrap();
        assert_eq!(inner.approvers.keys().collect::<Vec<_>>(), ["other"]);
        assert_eq!(inner.expiries.len(), 1);
    }

    fn part(cursor: i64, bytes: usize) -> Served {
        Served::new(
            "Bob",
            cursor,
            cursor,
            vec![
                json!({"call_id":"c1","gates":["auto"],"arguments":{"command":"x".repeat(bytes)}}),
            ],
            None,
        )
    }

    #[tokio::test]
    async fn a_burst_larger_than_the_output_waits_for_a_reading_holder() {
        use tokio::io::AsyncBufReadExt;
        let hub = Hub::default();
        let (writer, reader) = tokio::io::duplex(1 << 16);
        let output = Output::writer(writer);
        let (_, serving, _) = hub.serve("auto", 1, output.clone(), 60_000).unwrap();
        serving.lock().unwrap().go_live(0);
        // 24 parts of 200 KiB, more than twice the session's 2 MiB queue,
        // published at once as one group commit would.
        for cursor in 1..=24 {
            hub.approval(&part(cursor, 200 * 1024));
        }
        let mut lines = tokio::io::BufReader::new(reader).lines();
        for cursor in 1..=24 {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let pushed: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(pushed["cursor"], cursor);
        }
        assert!(!*output.subscribe_closed().borrow());
        assert_eq!(hub.served(), ["auto"]);
    }

    /// Serve `auto` on a session whose reader is not reading yet, and push
    /// more parts than its output holds, so some wait in the feed.
    async fn backed_up(hub: &Hub) -> (Output, tokio::io::DuplexStream, u64) {
        let (writer, reader) = tokio::io::duplex(1 << 16);
        let output = Output::writer(writer);
        let (lease, serving, _) = hub.serve("auto", 1, output.clone(), 60_000).unwrap();
        serving.lock().unwrap().go_live(0);
        for cursor in 1..=24 {
            hub.approval(&part(cursor, 200 * 1024));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        (output, reader, lease)
    }

    async fn read(reader: tokio::io::DuplexStream, until: i64) -> Vec<(u64, i64)> {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(reader).lines();
        let mut pushed = Vec::new();
        loop {
            let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let message: Value = serde_json::from_str(&line).unwrap();
            let cursor = message["cursor"].as_i64().unwrap();
            pushed.push((message["lease"].as_u64().unwrap(), cursor));
            if cursor == until {
                return pushed;
            }
        }
    }

    #[tokio::test]
    async fn serving_again_drops_the_calls_the_old_lease_had_waiting() {
        let hub = Hub::default();
        let (output, reader, old) = backed_up(&hub).await;
        let (new, serving, before) = hub.serve("auto", 1, output, 60_000).unwrap();
        serving.lock().unwrap().go_live(24);
        hub.replaced(before);
        hub.approval(&part(25, 16));
        // What the session's output already held goes out; the rest of the
        // old lease's calls are on the new listing, so they do not.
        let pushed = read(reader, 25).await;
        assert_eq!(pushed.last(), Some(&(new, 25)));
        let stale = pushed.iter().filter(|(lease, _)| *lease == old).count();
        assert!(stale < 24, "{pushed:?}");
    }

    #[tokio::test]
    async fn an_ended_lease_stops_a_push_still_waiting_for_room() {
        let hub = Hub::default();
        let (writer, reader) = tokio::io::duplex(1 << 16);
        let output = Output::writer(writer);
        let (_, serving, _) = hub.serve("auto", 1, output.clone(), 60_000).unwrap();
        serving.lock().unwrap().go_live(0);
        // Two parts of 900 KiB fill the session's 2 MiB output; the third
        // waits for room.
        for cursor in 1..=3 {
            hub.approval(&part(cursor, 900 * 1024));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (new, serving, before) = hub.serve("auto", 1, output, 60_000).unwrap();
        serving.lock().unwrap().go_live(3);
        hub.replaced(before);
        hub.approval(&part(4, 16));
        let cursors: Vec<i64> = read(reader, 4).await.into_iter().map(|(_, c)| c).collect();
        assert_eq!(
            cursors,
            [1, 2, 4],
            "the waiting part was on the new listing"
        );
        assert_eq!(
            hub.inner.lock().unwrap().approvers["auto"]
                .lock()
                .unwrap()
                .lease,
            new
        );
    }

    #[tokio::test]
    async fn a_failed_serve_puts_back_the_old_lease_and_its_calls() {
        let hub = Hub::default();
        let (output, reader, old) = backed_up(&hub).await;
        let (new, _, before) = hub.serve("auto", 1, output, 60_000).unwrap();
        hub.unserve("auto", new, before);
        let pushed = read(reader, 24).await;
        assert_eq!(
            pushed,
            (1..=24).map(|cursor| (old, cursor)).collect::<Vec<_>>()
        );
        assert_eq!(hub.served(), ["auto"]);
    }

    #[tokio::test]
    async fn a_holder_that_stopped_reading_is_closed() {
        let hub = Hub::default();
        let (writer, _unread) = tokio::io::duplex(1024);
        let output = Output::writer(writer);
        let (_, serving, _) = hub.serve("auto", 1, output.clone(), 60_000).unwrap();
        serving.lock().unwrap().go_live(0);
        for cursor in 1..=400 {
            hub.approval(&part(cursor, 16));
            tokio::task::yield_now().await;
        }
        assert!(*output.subscribe_closed().borrow());
        assert!(hub.served().is_empty());
    }
}
