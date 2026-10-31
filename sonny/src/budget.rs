//! The processes Sonny runs for agents at once: the tool host's tool calls
//! in `sonny tool` processes, and the lookout's goal checks. One total, as a
//! systemd slice bounds a group of units, so a turn that parks cheaply in
//! the daemon cannot leave unbounded work running here. It is split by a
//! fixed formula, as Kubernetes API Priority and Fairness gives each
//! priority level its share of one server's seats: the tool host, whose
//! calls hold live turns, takes three quarters, and the lookout, whose
//! checks run after turns end, a quarter, each at least one. So a burst of
//! slow checks never holds up a tool call, and the two companions, separate
//! processes, share nothing to keep the total. Callers wait their turn in
//! order (Tokio's semaphore is fair), and no agent holds more than half of
//! a companion's share, so one agent's burst leaves room for the rest.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct Budget {
    all: Arc<Semaphore>,
    /// Each agent's share while it holds or waits for a place, with how
    /// many hold or wait; gone once none does.
    shares: Mutex<HashMap<String, (Arc<Semaphore>, usize)>>,
    share: usize,
}

/// A place in the budget, given back when dropped.
pub struct Permit {
    _all: OwnedSemaphorePermit,
    _share: OwnedSemaphorePermit,
    _claim: Claim,
}

/// One holder of, or waiter on, an agent's share. Dropped, whether its
/// place came or its wait was given up, it leaves no unused share behind.
struct Claim {
    owner: String,
    budget: &'static Budget,
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut shares = self.budget.shares.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, users)) = shares.get_mut(&self.owner) {
            *users -= 1;
            if *users == 0 {
                shares.remove(&self.owner);
            }
        }
    }
}

impl Budget {
    pub fn new(size: usize) -> Self {
        let size = size.max(1);
        Budget {
            all: Arc::new(Semaphore::new(size)),
            shares: Mutex::new(HashMap::new()),
            share: size.div_ceil(2),
        }
    }

    /// Wait for a place for `owner`: its share first, so an agent waiting
    /// on its own share holds none of the others'. Cancel-safe.
    pub async fn admit(&'static self, owner: &str) -> Permit {
        let (share, claim) = {
            let mut shares = self.shares.lock().unwrap_or_else(|e| e.into_inner());
            let (share, users) = shares
                .entry(owner.to_owned())
                .or_insert_with(|| (Arc::new(Semaphore::new(self.share)), 0));
            *users += 1;
            let claim = Claim {
                owner: owner.to_owned(),
                budget: self,
            };
            (share.clone(), claim)
        };
        let share = share.acquire_owned().await.expect("never closed");
        let all = self
            .all
            .clone()
            .acquire_owned()
            .await
            .expect("never closed");
        Permit {
            _all: all,
            _share: share,
            _claim: claim,
        }
    }
}

/// Which of Sonny's companions a budget is for.
#[derive(Clone, Copy)]
pub enum Companion {
    ToolHost,
    Lookout,
}

/// `companion`'s share of `total`: the lookout a quarter, the tool host the
/// rest, each at least one.
fn share(total: usize, companion: Companion) -> usize {
    let lookout = (total / 4).max(1);
    match companion {
        Companion::Lookout => lookout,
        Companion::ToolHost => total.saturating_sub(lookout).max(1),
    }
}

/// This companion's budget: its share of `tool_processes` in the config,
/// or of four times the CPUs.
pub fn processes(companion: Companion) -> &'static Budget {
    static TOOL_HOST: OnceLock<Budget> = OnceLock::new();
    static LOOKOUT: OnceLock<Budget> = OnceLock::new();
    let budget = match companion {
        Companion::ToolHost => &TOOL_HOST,
        Companion::Lookout => &LOOKOUT,
    };
    budget.get_or_init(|| {
        let set = agent_client::config::load().map_or(0, |config| config.sonny.tool_processes);
        let cpus = std::thread::available_parallelism().map_or(4, |n| n.get());
        let total = match set {
            0 => 4 * cpus,
            set => usize::try_from(set).unwrap_or(usize::MAX),
        };
        Budget::new(share(total, companion))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn leaked(size: usize) -> &'static Budget {
        Box::leak(Box::new(Budget::new(size)))
    }

    #[test]
    fn the_tool_host_takes_three_quarters_and_the_lookout_a_quarter() {
        let split = |total| {
            (
                share(total, Companion::ToolHost),
                share(total, Companion::Lookout),
            )
        };
        assert_eq!(split(48), (36, 12));
        assert_eq!(split(10), (8, 2));
        assert_eq!(split(4), (3, 1));
        // Each gets at least one, even past a total too small for both.
        assert_eq!(split(2), (1, 1));
        assert_eq!(split(1), (1, 1));
    }

    #[tokio::test]
    async fn one_agent_takes_at_most_half_and_the_rest_get_in() {
        let budget = leaked(4);
        let a1 = budget.admit("a").await;
        let _a2 = budget.admit("a").await;
        // A third for `a` waits on its share, holding none of the others'.
        let third = tokio::spawn(async move { budget.admit("a").await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!third.is_finished());
        let _b1 = budget.admit("b").await;
        let _b2 = budget.admit("b").await;
        drop(a1);
        let _a3 = tokio::time::timeout(Duration::from_secs(1), third)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn an_agent_holding_none_leaves_no_share_behind() {
        let budget = leaked(2);
        drop(budget.admit("a").await);
        assert!(budget.shares.lock().unwrap().is_empty());
        let held = budget.admit("a").await;
        let second = budget.admit("a");
        drop(held);
        drop(second.await);
        assert!(budget.shares.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_agent_that_stops_waiting_leaves_no_share_behind() {
        let budget = leaked(2);
        let _a = budget.admit("a").await;
        let _b = budget.admit("b").await;
        // The budget is full: `c` waits, then gives up.
        let gave_up = tokio::time::timeout(Duration::from_millis(20), budget.admit("c")).await;
        assert!(gave_up.is_err());
        assert!(!budget.shares.lock().unwrap().contains_key("c"));
    }
}
