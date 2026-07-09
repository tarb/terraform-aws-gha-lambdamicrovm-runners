//! The new-run/cleaning handshake between `/run` tasks and the pool idle
//! waiter.
//!
//! Every `/run` bumps a generation counter; an idle waiter snapshots the
//! counter when it starts watching and considers any later bump "a new run
//! took over". Because a snapshot can never erase an increment, a resume
//! `/run` racing post-job cleanup structurally cannot lose its signal — and
//! the waiter still re-checks at grace expiry as the last-line "never
//! terminate a VM with an active run" guard.
//!
//! `cleaning` orders a racing `/run` behind the previous cycle's teardown:
//! the cleanup holds a [`CleaningGuard`] (RAII, panic-safe) and the new run
//! waits — bounded — for it to drop before registering.

use std::time::Duration;
use tokio::sync::watch;

pub struct PoolGate {
    runs: watch::Sender<u64>,
    cleaning: watch::Sender<bool>,
}

impl PoolGate {
    pub fn new() -> Self {
        Self {
            runs: watch::Sender::new(0),
            cleaning: watch::Sender::new(false),
        }
    }

    /// A `/run` arrived: any live or future watcher of an older generation
    /// sees it.
    pub fn announce_run(&self) {
        self.runs.send_modify(|generation| *generation += 1);
    }

    /// Snapshot the current run generation; later announcements are "new".
    pub fn watch_runs(&self) -> RunWatch {
        let rx = self.runs.subscribe();
        let seen = *rx.borrow();
        RunWatch { seen, rx }
    }

    /// Wait for an in-flight cleanup to finish, but never longer than `max`
    /// — cleanup is seconds of work, and a wedged cleanup must not block a
    /// live job forever.
    pub async fn await_cleaning_done(&self, max: Duration) {
        let mut rx = self.cleaning.subscribe();
        let _ = tokio::time::timeout(max, rx.wait_for(|cleaning| !cleaning)).await;
    }

    /// Mark cleanup in progress until the returned guard drops (including
    /// by panic unwind).
    pub fn begin_cleaning(&self) -> CleaningGuard {
        self.cleaning.send_replace(true);
        CleaningGuard {
            cleaning: self.cleaning.clone(),
        }
    }
}

impl Default for PoolGate {
    fn default() -> Self {
        Self::new()
    }
}

/// One idle waiter's view of the run generation.
pub struct RunWatch {
    seen: u64,
    rx: watch::Receiver<u64>,
}

impl RunWatch {
    /// True the moment a run newer than the snapshot is announced (including
    /// announcements that happened before this call — level-triggered), or
    /// false after `tick`. Does not advance the snapshot.
    pub async fn new_run_within(&mut self, tick: Duration) -> bool {
        let seen = self.seen;
        let outcome = {
            let waited =
                tokio::time::timeout(tick, self.rx.wait_for(|generation| *generation != seen))
                    .await;
            match waited {
                Ok(Ok(_)) => Some(true),
                // Gate dropped: nothing can announce anymore.
                Ok(Err(_)) => None,
                Err(_) => Some(false),
            }
        };
        match outcome {
            Some(saw_run) => saw_run,
            None => {
                // Consume the tick so the polling loop keeps its cadence.
                tokio::time::sleep(tick).await;
                false
            }
        }
    }

    /// Has any run been announced since the snapshot? The grace-expiry
    /// stand-down check.
    pub fn new_run_seen(&self) -> bool {
        *self.rx.borrow() != self.seen
    }
}

/// RAII cleanup marker; dropping it (normally or during unwind) releases
/// waiting `/run` tasks.
pub struct CleaningGuard {
    cleaning: watch::Sender<bool>,
}

impl Drop for CleaningGuard {
    fn drop(&mut self) {
        self.cleaning.send_replace(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn announce_before_wait_is_seen_immediately() {
        let gate = PoolGate::new();
        let mut watch = gate.watch_runs();
        gate.announce_run();
        assert!(watch.new_run_within(Duration::from_secs(5)).await);
        assert!(watch.new_run_seen());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_times_out_false_without_a_run() {
        let gate = PoolGate::new();
        let mut watch = gate.watch_runs();
        assert!(!watch.new_run_within(Duration::from_millis(100)).await);
        assert!(!watch.new_run_seen());
    }

    #[tokio::test(start_paused = true)]
    async fn announce_during_wait_wakes_the_waiter() {
        let gate = Arc::new(PoolGate::new());
        let mut watch = gate.watch_runs();
        let announcer = gate.clone();
        let waiter =
            tokio::spawn(async move { watch.new_run_within(Duration::from_secs(60)).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        announcer.announce_run();
        assert!(waiter.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn announce_between_snapshots_is_never_lost() {
        // A snapshot cannot erase an increment: the waiter that was live
        // when the run arrived sees it (the grace-expiry stand-down
        // predicate), while a waiter that started after does not.
        let gate = PoolGate::new();
        let earlier = gate.watch_runs();
        gate.announce_run();
        let later = gate.watch_runs();
        assert!(earlier.new_run_seen(), "stand-down check must fire");
        assert!(!later.new_run_seen());
    }

    #[tokio::test(start_paused = true)]
    async fn cleaning_guard_releases_on_drop_and_on_panic() {
        let gate = Arc::new(PoolGate::new());
        {
            let _guard = gate.begin_cleaning();
            assert!(*gate.cleaning.borrow());
        }
        assert!(!*gate.cleaning.borrow());

        let panicking = gate.clone();
        let task = tokio::spawn(async move {
            let _guard = panicking.begin_cleaning();
            panic!("cleanup blew up");
        });
        assert!(task.await.is_err());
        assert!(!*gate.cleaning.borrow(), "guard must release on unwind");
    }

    #[tokio::test(start_paused = true)]
    async fn await_cleaning_done_is_bounded() {
        let gate = PoolGate::new();
        let _guard = gate.begin_cleaning();
        // Never released: the wait must still return at the bound.
        gate.await_cleaning_done(Duration::from_secs(30)).await;
    }

    #[tokio::test(start_paused = true)]
    async fn await_cleaning_done_returns_immediately_when_idle() {
        let gate = PoolGate::new();
        gate.await_cleaning_done(Duration::from_secs(30)).await;
    }
}
