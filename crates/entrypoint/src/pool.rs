//! Warm-pool lifecycle — port of `_pool_cleanup` and `_pool_idle_wait`,
//! including the stand-down guard against the cleanup/resume race and the
//! final claim attempt at grace expiry.

use crate::aws::{claim_handoff, terminate_self};
use crate::config::env_or;
use crate::payload::Payload;
use crate::runner::{job_running, spawn_start_runner};
use crate::state::Sup;
use crate::util::{log, py_truthy, python_int};
use std::sync::Arc;
use std::time::Duration;

/// Wall/monotonic clock pair, injectable for tests.
pub trait Clock: Send + Sync {
    /// `time.monotonic()` (seconds).
    fn monotonic(&self) -> f64;
    /// `time.time()` (seconds).
    fn wall(&self) -> f64;
}

pub struct RealClock(std::time::Instant);

impl RealClock {
    pub fn new() -> Self {
        Self(std::time::Instant::now())
    }
}

impl Default for RealClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for RealClock {
    fn monotonic(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }

    fn wall(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }
}

/// The pure deadline arithmetic of `_pool_idle_wait`: a wall-clock jump of
/// more than 60s beyond monotonic progress means we were suspended and
/// resumed — reset the grace deadline so a pre-suspend deadline can't kill
/// us early.
pub struct IdleWait {
    grace: f64,
    deadline: f64,
    last_mono: f64,
    last_real: f64,
}

impl IdleWait {
    pub fn new(grace: f64, clock: &dyn Clock) -> Self {
        let (mono, real) = (clock.monotonic(), clock.wall());
        Self {
            grace,
            deadline: mono + grace,
            last_mono: mono,
            last_real: real,
        }
    }

    /// `while time.monotonic() < deadline` — expired at/after the deadline.
    pub fn expired(&self, clock: &dyn Clock) -> bool {
        clock.monotonic() >= self.deadline
    }

    /// One loop tick's clock observation. Returns true when a resume
    /// (wall-clock jump) was detected — the deadline has then been reset.
    pub fn observe(&mut self, clock: &dyn Clock) -> bool {
        let (mono, real) = (clock.monotonic(), clock.wall());
        let jumped = (real - self.last_real) - (mono - self.last_mono) > 60.0;
        if jumped {
            self.deadline = mono + self.grace;
        }
        self.last_mono = mono;
        self.last_real = real;
        jumped
    }
}

/// Between pooled jobs: tear down dockerd AND its orphaned runtimes (a
/// resumed daemon has snapshot-stale networking), wipe the runner workdir
/// and any config.sh registration residue. The `cleaning` flag orders this
/// against a racing new /run.
pub async fn pool_cleanup(sup: &Sup) {
    sup.cleaning.set();
    {
        let mut docker = sup.docker.lock().await;
        if let Some(mut child) = docker.child.take() {
            let teardown: Result<(), String> = async {
                if child.try_wait().map_err(|e| e.to_string())?.is_none() {
                    if let Some(pid) = child.id() {
                        unsafe {
                            let _ = libc::kill(pid as i32, libc::SIGTERM);
                        }
                    }
                    tokio::time::timeout(Duration::from_secs(15), child.wait())
                        .await
                        .map_err(|_| "dockerd did not exit within 15s".to_string())?
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            }
            .await;
            if let Err(e) = teardown {
                log(format!("pool cleanup: dockerd teardown: {e}"));
            }
        }
        docker.ready = false;
    }
    if let Err(e) = crate::docker::kill_stale_runtimes().await {
        log(format!("pool cleanup: stale runtimes: {e}"));
    }
    let _ = std::fs::remove_dir_all(format!("{}/_work", sup.cfg.runner_dir));
    for residue in [".runner", ".credentials", ".credentials_rsaparams"] {
        let _ = std::fs::remove_file(format!("{}/{residue}", sup.cfg.runner_dir));
    }
    sup.cleaning.clear();
}

/// After a pooled job: wait to be SUSPENDED by the dispatcher (we cannot
/// self-suspend). On resume, poll the SSM mailbox every tick. If nothing
/// suspends or reuses us in time, self-terminate: an unsuspended idle VM
/// bills at full rate.
pub async fn pool_idle_wait(sup: Arc<Sup>, payload: &Payload) -> Result<(), String> {
    let grace: i64 = match payload.get("pool_grace") {
        Some(v) if py_truthy(v) => python_int(v)?,
        _ => env_or("POOL_SUSPEND_GRACE_SECONDS", "300")
            .parse()
            .map_err(|_| "POOL_SUSPEND_GRACE_SECONDS must be an integer".to_string())?,
    };
    sup.new_run.clear();
    let clock = RealClock::new();
    let mut wait = IdleWait::new(grace as f64, &clock);
    while !wait.expired(&clock) {
        // The event fires the INSTANT a new /run arrives — a resumed VM with
        // a job in flight can never lose the race to a stale deadline.
        if sup.new_run.wait_timeout(Duration::from_secs(5)).await {
            log("pool: new run took over");
            return Ok(());
        }
        if let Some(new_payload) = claim_handoff(&*sup.aws, payload).await
            && py_truthy(&new_payload)
        {
            log("pool: handoff claimed - starting new run");
            spawn_start_runner(sup.clone(), Payload::from_value(new_payload));
            return Ok(()); // start_runner sets new_run; this watch is over
        }
        if wait.observe(&clock) {
            // Resumed: the handoff poll picks the job up on this or the next
            // tick; just don't let a pre-suspend deadline kill us early.
            log("pool: resume detected (wall-clock jump)");
        }
    }
    // Stand down instead of terminating if a new run claimed the VM while
    // this waiter was blind — a resume /run can race the post-job cleanup
    // (suspend freezes the guest MID-cleanup; on thaw, this task's
    // new_run.clear() above can wipe the incoming run's set()). Killing the
    // box here would fail a live job.
    if sup.new_run.is_set() || job_running() {
        log("pool: run active at grace expiry - standing down");
        return Ok(());
    }
    // If monotonic time ADVANCED across a suspension, the deadline can
    // expire the instant we thaw — with our next job already parked. One
    // last look before dying.
    if let Some(new_payload) = claim_handoff(&*sup.aws, payload).await
        && py_truthy(&new_payload)
    {
        log("pool: handoff claimed at grace expiry - starting new run");
        spawn_start_runner(sup.clone(), Payload::from_value(new_payload));
        return Ok(());
    }
    log(format!(
        "pool: not suspended or reused within {grace}s - self-terminating"
    ));
    terminate_self(&*sup.aws, payload, &sup.region_label).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Injectable mock clock: monotonic and wall time settable independently
    /// to simulate suspends (wall jumps ahead of monotonic).
    struct MockClock {
        now: Mutex<(f64, f64)>, // (monotonic, wall)
    }

    impl MockClock {
        fn new(mono: f64, wall: f64) -> Self {
            Self {
                now: Mutex::new((mono, wall)),
            }
        }

        fn advance(&self, mono: f64, wall: f64) {
            let mut n = self.now.lock().unwrap();
            n.0 += mono;
            n.1 += wall;
        }
    }

    impl Clock for MockClock {
        fn monotonic(&self) -> f64 {
            self.now.lock().unwrap().0
        }

        fn wall(&self) -> f64 {
            self.now.lock().unwrap().1
        }
    }

    #[test]
    fn expires_after_grace_of_plain_monotonic_time() {
        let clock = MockClock::new(100.0, 1_000_000.0);
        let mut wait = IdleWait::new(300.0, &clock);
        assert!(!wait.expired(&clock));
        // Ticks where wall and monotonic advance together: no jump.
        for _ in 0..59 {
            clock.advance(5.0, 5.0);
            assert!(!wait.observe(&clock), "no jump expected");
        }
        assert!(!wait.expired(&clock)); // t=395 < 400
        clock.advance(5.0, 5.0);
        assert!(wait.expired(&clock)); // t=400 >= deadline 400
    }

    #[test]
    fn wall_clock_jump_resets_the_deadline() {
        let clock = MockClock::new(0.0, 1_000_000.0);
        let mut wait = IdleWait::new(300.0, &clock);
        clock.advance(290.0, 290.0);
        assert!(!wait.observe(&clock));
        // Suspended for an hour: wall leaps 3600s while monotonic ticks 5s.
        clock.advance(5.0, 3600.0);
        assert!(wait.observe(&clock), "resume must be detected");
        // Old deadline (300) has passed (mono=295)... but the reset buys a
        // full new grace window from mono=295.
        assert!(!wait.expired(&clock));
        clock.advance(294.0, 294.0);
        assert!(!wait.expired(&clock)); // 589 < 595
        clock.advance(6.0, 6.0);
        assert!(wait.expired(&clock)); // 595 >= 595
    }

    #[test]
    fn jump_must_exceed_sixty_seconds() {
        let clock = MockClock::new(0.0, 0.0);
        let mut wait = IdleWait::new(300.0, &clock);
        // Wall drifts exactly 60s ahead: NOT a jump (Python uses `> 60`).
        clock.advance(5.0, 65.0);
        assert!(!wait.observe(&clock));
        // A tick whose drift exceeds 60s: jump.
        clock.advance(5.0, 66.0);
        assert!(wait.observe(&clock));
    }

    #[test]
    fn monotonic_advancing_past_wall_never_resets() {
        // Monotonic advancing across a suspension (the grace-expiry case the
        // Python comments call out): diff is negative, no reset.
        let clock = MockClock::new(0.0, 0.0);
        let mut wait = IdleWait::new(100.0, &clock);
        clock.advance(3600.0, 5.0);
        assert!(!wait.observe(&clock));
        assert!(wait.expired(&clock));
    }

    #[test]
    fn jump_detection_is_relative_to_last_tick_not_start() {
        let clock = MockClock::new(0.0, 0.0);
        let mut wait = IdleWait::new(300.0, &clock);
        // Many small drifts that each stay under 60s must never trigger.
        for _ in 0..20 {
            clock.advance(5.0, 35.0); // +30s drift per tick
            assert!(!wait.observe(&clock));
        }
    }
}
