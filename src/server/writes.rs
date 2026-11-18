//! The write lane: a client's write that only records what it says (an
//! answer to a call or a gate, a call taken, a step put to its gates or
//! recorded) goes to the storage worker from its own task, and the run
//! loop answers it once its commit lands. As the observation lane does for
//! a read, its session is parked meanwhile, so the session's next request
//! never overtakes its write, while every other session goes on: a slow
//! commit holds the writer, not the loop. Writes are answered in the order
//! they were sent, whichever session sent them, as when each was awaited
//! in turn: a call taken or a step allowed is answered before an answer to
//! that call sent after it.
use std::collections::VecDeque;
use tokio::task::JoinHandle;

/// Writes on their way to the storage worker, oldest first: each one's
/// session, what the run loop keeps to answer it (`T`), and its task,
/// which gives what the write returned (`D`).
pub struct Writes<T, D> {
    running: VecDeque<(u64, T, JoinHandle<D>)>,
    /// Sessions given a write since the run loop last looked: it parks them.
    parked: Vec<u64>,
}

impl<T, D> Default for Writes<T, D> {
    fn default() -> Self {
        Writes {
            running: VecDeque::new(),
            parked: Vec::new(),
        }
    }
}

impl<T, D> Drop for Writes<T, D> {
    fn drop(&mut self) {
        for (_, _, task) in &self.running {
            task.abort();
        }
    }
}

impl<T, D: Send + 'static> Writes<T, D> {
    /// Run `write` for `session` in its own task, keeping `then` for its
    /// answer; the session is parked until `next` gives it back.
    pub fn write(
        &mut self,
        session: u64,
        then: T,
        write: impl Future<Output = D> + Send + 'static,
    ) {
        self.parked.push(session);
        self.running.push_back((session, then, tokio::spawn(write)));
    }

    /// The sessions given a write since the last call.
    pub fn parked(&mut self) -> std::vec::Drain<'_, u64> {
        self.parked.drain(..)
    }

    /// No write on its way.
    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    /// The oldest write, once its task ended: its session, what was kept
    /// for it, and what it returned, or `None` when its task failed.
    /// Cancel-safe.
    pub async fn next(&mut self) -> Option<(u64, T, Option<D>)> {
        let (_, _, task) = self.running.front_mut()?;
        let done = task.await.ok();
        let (session, then, _) = self.running.pop_front().expect("the write just awaited");
        Some((session, then, done))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn writes_are_given_back_with_their_sessions_in_the_order_they_were_sent() {
        let mut writes = Writes::<&str, u32>::default();
        let (held, gate) = oneshot::channel::<()>();
        writes.write(1, "first", async move {
            let _ = gate.await;
            1
        });
        writes.write(2, "second", async { 2 });
        assert_eq!(writes.parked().collect::<Vec<_>>(), [1, 2]);
        // The second lands while the first is held: it waits behind it.
        tokio::task::yield_now().await;
        let early = tokio::time::timeout(Duration::from_millis(20), writes.next()).await;
        assert!(early.is_err(), "a later write overtook a held one");
        held.send(()).unwrap();
        assert_eq!(writes.next().await.unwrap(), (1, "first", Some(1)));
        assert_eq!(writes.next().await.unwrap(), (2, "second", Some(2)));
        assert!(writes.is_empty());
        assert!(writes.next().await.is_none());
    }

    #[tokio::test]
    async fn a_write_whose_task_failed_is_still_given_back() {
        let mut writes = Writes::<u64, u32>::default();
        writes.write(7, 70, async { panic!("a broken write") });
        assert_eq!(writes.next().await.unwrap(), (7, 70, None));
    }
}
