//! The observation lane: reads that decide nothing (history, turns,
//! listings, usage) run beside the service loop instead of inside it, so a
//! deep history read never holds another session's submit, interrupt or
//! pause behind it. As Kubernetes' API Priority and Fairness gives each
//! priority level its own seats: at most `limit` run at once on the store's
//! reader pool (all but one of its connections, so turns' own reads always
//! find one free), and the rest wait in the order they came. A session waits for its
//! own read before its next request is taken (the run loop parks it), so
//! each session's requests keep their order, every session has at most one
//! read here, and sessions waiting for a seat are served in turn.
use agent_runtime::{Error, Result, output::Output};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use tokio::task::{Id, JoinSet};

/// Who asked for a read, and where its answer goes.
pub struct Asker {
    pub session: u64,
    pub output: Output,
    pub id: Value,
}

/// Sends a read's answer to its asker, saying whether it was sent. It runs
/// in the read's own task: a page of history can be megabytes to write out,
/// which the service loop does not wait for either.
pub type Deliver = fn(u64, Output, Value, Result<Value>) -> BoxFuture<'static, bool>;

pub struct Lane {
    running: JoinSet<bool>,
    askers: HashMap<Id, Asker>,
    waiting: VecDeque<(Asker, BoxFuture<'static, Result<Value>>)>,
    /// Sessions given a read since the run loop last looked: it parks them.
    parked: Vec<u64>,
    limit: usize,
    deliver: Deliver,
}

impl Lane {
    pub fn new(limit: usize, deliver: Deliver) -> Self {
        Lane {
            running: JoinSet::new(),
            askers: HashMap::new(),
            waiting: VecDeque::new(),
            parked: Vec::new(),
            limit: limit.max(1),
            deliver,
        }
    }

    /// Run `read` for `asker` once a seat is free, and answer it; its
    /// session is parked until `next` says it was answered.
    pub fn observe(
        &mut self,
        asker: Asker,
        read: impl Future<Output = Result<Value>> + Send + 'static,
    ) {
        self.parked.push(asker.session);
        self.waiting.push_back((asker, Box::pin(read)));
        self.seat();
    }

    /// The sessions given a read since the last call.
    pub fn parked(&mut self) -> std::vec::Drain<'_, u64> {
        self.parked.drain(..)
    }

    /// No read running or waiting.
    pub fn is_empty(&self) -> bool {
        self.running.is_empty() && self.waiting.is_empty()
    }

    /// The next read answered, who asked for it, and whether the answer
    /// reached them; the next waiting read takes its seat. Cancel-safe.
    pub async fn next(&mut self) -> Option<(Asker, bool)> {
        let joined = self.running.join_next_with_id().await?;
        let (task, sent) = match joined {
            Ok((task, sent)) => (task, Some(sent)),
            Err(error) => (error.id(), None),
        };
        let asker = self.askers.remove(&task).expect("every read has its asker");
        // A task that failed answered nothing: a small refusal says so.
        let sent = sent.unwrap_or_else(|| {
            let failed = Err(Error::new("observation_failed"));
            asker.output.try_respond(asker.id.clone(), failed).is_ok()
        });
        self.seat();
        Some((asker, sent))
    }

    /// Stop every read: at shutdown, answering is no longer owed, and the
    /// outputs they hold must go before stdout joins.
    pub async fn stop(&mut self) {
        self.waiting.clear();
        self.running.abort_all();
        while self.running.join_next().await.is_some() {}
        self.askers.clear();
    }

    fn seat(&mut self) {
        while self.running.len() < self.limit
            && let Some((asker, read)) = self.waiting.pop_front()
        {
            let (session, output, id) = (asker.session, asker.output.clone(), asker.id.clone());
            let deliver = self.deliver;
            let task = self
                .running
                .spawn(async move { deliver(session, output, id, read.await).await })
                .id();
            self.askers.insert(task, asker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// What each test's reads answered, by session.
    static ANSWERED: Mutex<Vec<(u64, std::result::Result<Value, String>)>> = Mutex::new(Vec::new());

    fn noted(session: u64, _: Output, _: Value, result: Result<Value>) -> BoxFuture<'static, bool> {
        ANSWERED
            .lock()
            .unwrap()
            .push((session, result.map_err(|e| e.code)));
        Box::pin(async { true })
    }

    fn asker(session: u64) -> Asker {
        Asker {
            session,
            output: Output::writer(tokio::io::sink()),
            id: Value::from(session),
        }
    }

    fn answered(session: u64) -> Vec<std::result::Result<Value, String>> {
        let answered = ANSWERED.lock().unwrap();
        (answered.iter())
            .filter(|(asked, _)| *asked == session)
            .map(|(_, answer)| answer.clone())
            .collect()
    }

    #[tokio::test]
    async fn reads_past_the_seats_wait_in_order_and_each_answer_names_its_asker() {
        let mut lane = Lane::new(1, noted);
        let (first, held) = oneshot::channel::<()>();
        lane.observe(asker(1), async move {
            let _ = held.await;
            Ok(Value::from("one"))
        });
        lane.observe(asker(2), async { Ok(Value::from("two")) });
        assert_eq!(lane.parked().collect::<Vec<_>>(), [1, 2]);
        // The second waits for the first's seat.
        let early = tokio::time::timeout(Duration::from_millis(20), lane.next()).await;
        assert!(early.is_err(), "a read ran past the lane's seats");
        assert!(answered(2).is_empty());
        first.send(()).unwrap();
        let (asker, sent) = lane.next().await.unwrap();
        assert_eq!((asker.session, sent), (1, true));
        assert_eq!(answered(1), [Ok(Value::from("one"))]);
        let (asker, sent) = lane.next().await.unwrap();
        assert_eq!((asker.session, sent), (2, true));
        assert_eq!(answered(2), [Ok(Value::from("two"))]);
        assert!(lane.is_empty());
        assert!(lane.next().await.is_none());
    }

    #[tokio::test]
    async fn a_read_that_panics_still_answers_its_asker() {
        let mut lane = Lane::new(2, noted);
        let (daemon, client) = tokio::io::duplex(4096);
        let asked = Asker {
            session: 7,
            output: Output::writer(daemon),
            id: Value::from(70),
        };
        lane.observe(asked, async { panic!("a broken read") });
        let (asker, sent) = lane.next().await.unwrap();
        assert_eq!((asker.session, sent), (7, true));
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(client));
        let line = lines.next_line().await.unwrap().unwrap();
        let answer: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            (&answer["id"], &answer["error"]),
            (&Value::from(70), &Value::from("observation_failed"))
        );
    }
}
