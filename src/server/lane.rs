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

pub struct Lane {
    running: JoinSet<Result<Value>>,
    askers: HashMap<Id, Asker>,
    waiting: VecDeque<(Asker, BoxFuture<'static, Result<Value>>)>,
    /// Sessions given a read since the run loop last looked: it parks them.
    parked: Vec<u64>,
    limit: usize,
}

impl Default for Lane {
    fn default() -> Self {
        Self::new(2)
    }
}

impl Lane {
    pub fn new(limit: usize) -> Self {
        Lane {
            running: JoinSet::new(),
            askers: HashMap::new(),
            waiting: VecDeque::new(),
            parked: Vec::new(),
            limit: limit.max(1),
        }
    }

    /// Run `read` for `asker` once a seat is free; its session is parked
    /// until the answer comes back from `next`.
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

    /// The next read answered, and who asked for it; the next waiting read
    /// takes its seat. Cancel-safe.
    pub async fn next(&mut self) -> Option<(Asker, Result<Value>)> {
        let joined = self.running.join_next_with_id().await?;
        let (task, result) = match joined {
            Ok((task, result)) => (task, result),
            Err(error) => (error.id(), Err(Error::new("observation_failed"))),
        };
        let asker = self.askers.remove(&task).expect("every read has its asker");
        self.seat();
        Some((asker, result))
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
            let task = self.running.spawn(read).id();
            self.askers.insert(task, asker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn asker(session: u64) -> Asker {
        Asker {
            session,
            output: Output::writer(tokio::io::sink()),
            id: Value::from(session),
        }
    }

    #[tokio::test]
    async fn reads_past_the_seats_wait_in_order_and_each_answer_names_its_asker() {
        let mut lane = Lane::new(1);
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
        first.send(()).unwrap();
        let (asker, answer) = lane.next().await.unwrap();
        assert_eq!((asker.session, answer.unwrap()), (1, Value::from("one")));
        let (asker, answer) = lane.next().await.unwrap();
        assert_eq!((asker.session, answer.unwrap()), (2, Value::from("two")));
        assert!(lane.is_empty());
        assert!(lane.next().await.is_none());
    }

    #[tokio::test]
    async fn a_read_that_panics_still_answers_its_asker() {
        let mut lane = Lane::new(2);
        lane.observe(asker(7), async { panic!("a broken read") });
        let (asker, answer) = lane.next().await.unwrap();
        assert_eq!(asker.session, 7);
        assert_eq!(answer.unwrap_err().code, "observation_failed");
    }
}
