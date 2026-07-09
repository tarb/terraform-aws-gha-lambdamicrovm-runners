//! Warm-pool lifecycle between jobs: teardown, then wait to be SUSPENDED by
//! the dispatcher (we cannot self-suspend), polling the handoff mailbox
//! while awake. Includes the stand-down guard against the cleanup/resume
//! race and the final claim attempt at grace expiry.

use crate::handoff::{self, HandoffAddress};
use crate::logfmt::log;
use crate::payload::{RunConfig, lenient};
use crate::state::AppState;
use crate::{docker, report, supervisor};
use std::sync::Arc;
use std::time::Duration;
use types::IdleReason;

/// Wall/monotonic clock pair, injectable for tests.
pub trait Clock: Send + Sync {
    /// Monotonic seconds since an arbitrary origin.
    fn monotonic(&self) -> f64;
    /// Unix wall-clock seconds.
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

/// The pure deadline arithmetic of the idle wait: a wall-clock jump of more
/// than 60 s beyond monotonic progress means we were suspended and resumed —
/// reset the grace deadline so a pre-suspend deadline can't kill us early.
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

    /// Expired at or after the deadline.
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
/// and any config.sh registration residue. The cleaning guard orders this
/// against a racing new /run.
pub async fn pool_cleanup(app: &AppState) {
    let _guard = app.gate.begin_cleaning();
    app.docker.teardown().await;
    if let Err(e) = docker::kill_stale_runtimes().await {
        log(format!("pool cleanup: stale runtimes: {e}"));
    }
    let _ = std::fs::remove_dir_all(format!("{}/_work", app.cfg.runner_dir));
    for residue in [".runner", ".credentials", ".credentials_rsaparams"] {
        let _ = std::fs::remove_file(format!("{}/{residue}", app.cfg.runner_dir));
    }
}

/// After a pooled job: wait to be SUSPENDED by the dispatcher. On resume,
/// poll the SSM mailbox every tick. If nothing suspends or reuses us in
/// time, self-terminate: an unsuspended idle VM bills at full rate.
pub async fn pool_idle_wait(app: Arc<AppState>, cfg: &RunConfig) {
    // Snapshot the run generation FIRST: any /run announced after this
    // point is "a new run took over".
    let watch = app.gate.watch_runs();
    idle_wait_with(app, cfg, watch).await;
}

async fn idle_wait_with(app: Arc<AppState>, cfg: &RunConfig, mut watch: crate::gate::RunWatch) {
    let grace = cfg.pool_grace_or_env();
    let clock = RealClock::new();
    let mut wait = IdleWait::new(grace as f64, &clock);
    let addr = HandoffAddress::from_run(cfg);
    while !wait.expired(&clock) {
        // Fires the INSTANT a new /run arrives — a resumed VM with a job in
        // flight can never lose the race to a stale deadline.
        if watch.new_run_within(Duration::from_secs(5)).await {
            log("pool: new run took over");
            return;
        }
        if let Some(addr) = &addr
            && let Some(payload) = handoff::claim(&*app.aws, addr).await
            && lenient::truthy(&payload)
        {
            log("pool: handoff claimed - starting new run");
            supervisor::spawn_run_task(app.clone(), RunConfig::from_value(payload));
            return; // the new run announces itself; this watch is over
        }
        if wait.observe(&clock) {
            // Resumed: the handoff poll picks the job up on this or the
            // next tick; just don't let a pre-suspend deadline kill us
            // early.
            log("pool: resume detected (wall-clock jump)");
        }
    }
    // Stand down instead of terminating if a new run claimed the VM while
    // this waiter was blind — a resume /run can race the post-job cleanup
    // (suspend freezes the guest MID-cleanup). Killing the box here would
    // fail a live job.
    if watch.new_run_seen() || (app.job_probe)() {
        log("pool: run active at grace expiry - standing down");
        return;
    }
    // If monotonic time ADVANCED across a suspension, the deadline can
    // expire the instant we thaw — with our next job already parked. One
    // last look before dying.
    if let Some(addr) = &addr
        && let Some(payload) = handoff::claim(&*app.aws, addr).await
        && lenient::truthy(&payload)
    {
        log("pool: handoff claimed at grace expiry - starting new run");
        supervisor::spawn_run_task(app.clone(), RunConfig::from_value(payload));
        return;
    }
    log(format!(
        "pool: not suspended or reused within {grace}s - reporting orphan"
    ));
    // Nothing suspended or reused us: report as an orphan — the dispatcher
    // terminates it from the control plane (we return right after this, so
    // pooling us would leave a suspended VM that could never claim a
    // handoff) — falling back to the in-VM self-terminate when reporting is
    // impossible.
    report::report_idle_or_terminate(&*app.aws, cfg, &app.region, IdleReason::Orphan).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::testsupport::test_app;
    use serde_json::json;
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
        // Wall drifts exactly 60s ahead: NOT a jump (strictly greater).
        clock.advance(5.0, 65.0);
        assert!(!wait.observe(&clock));
        // A tick whose drift exceeds 60s: jump.
        clock.advance(5.0, 66.0);
        assert!(wait.observe(&clock));
    }

    #[test]
    fn monotonic_advancing_past_wall_never_resets() {
        // Monotonic advancing across a suspension (the grace-expiry case
        // the final mailbox claim exists for): diff is negative, no reset.
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

    fn pool_cfg(grace: i64) -> RunConfig {
        RunConfig::from_value(json!({
            "pool": true,
            "pool_grace": grace,
            "microvmId": "microvm-x",
            "handoff_prefix": "/p",
        }))
    }

    #[tokio::test]
    async fn new_run_during_the_wait_takes_over() {
        let (app, fake) = test_app("/nonexistent");
        let cfg = pool_cfg(30);
        let watch = app.gate.watch_runs();
        app.gate.announce_run(); // a /run raced in after the snapshot
        idle_wait_with(app, &cfg, watch).await;
        assert!(fake.terminated.lock().unwrap().is_empty());
        assert!(
            fake.gets.lock().unwrap().is_empty(),
            "returned before polling"
        );
    }

    #[tokio::test]
    async fn run_announced_while_blind_stands_down_at_expiry() {
        let (app, fake) = test_app("/nonexistent");
        // Grace 0: the loop never runs; only the expiry re-check can save
        // the racing run (pool_grace 0 is falsy, so pin the env-independent
        // path by asserting the fallback... use an explicit tiny grace via
        // the payload instead).
        let cfg = RunConfig::from_value(json!({
            "pool": true, "pool_grace": "0", "microvmId": "microvm-x", "handoff_prefix": "/p",
        }));
        let watch = app.gate.watch_runs();
        app.gate.announce_run();
        // "0" is a truthy string that parses to 0: loop skipped entirely.
        idle_wait_with(app, &cfg, watch).await;
        assert!(
            fake.terminated.lock().unwrap().is_empty(),
            "must stand down"
        );
        assert!(
            fake.gets.lock().unwrap().is_empty(),
            "no final claim after stand-down"
        );
    }

    #[tokio::test]
    async fn final_claim_at_expiry_starts_the_parked_run() {
        let (app, fake) = test_app("/nonexistent");
        fake.params.lock().unwrap().insert(
            "/p/microvm-x".to_string(),
            Ok(Some("{\"github_url\": \"u\"}".to_string())),
        );
        let cfg = RunConfig::from_value(json!({
            "pool": true, "pool_grace": "0", "microvmId": "microvm-x", "handoff_prefix": "/p",
        }));
        let watch = app.gate.watch_runs();
        let mut new_run = app.gate.watch_runs();
        idle_wait_with(app.clone(), &cfg, watch).await;
        assert_eq!(fake.deletes.lock().unwrap().as_slice(), ["/p/microvm-x"]);
        assert!(fake.terminated.lock().unwrap().is_empty());
        // The claimed payload was handed to a fresh run task.
        assert!(new_run.new_run_within(Duration::from_secs(5)).await);
    }

    #[tokio::test]
    async fn live_job_at_expiry_stands_down_without_claiming() {
        // The CI-discovered case: no new /run announced, but a job IS
        // executing (on GitHub-hosted CI the real /proc probe finds the CI
        // runner's own Runner.Worker - hence the injected probe).
        let (app, fake) = test_app("/nonexistent");
        let mut app = app;
        std::sync::Arc::get_mut(&mut app)
            .expect("fresh state")
            .job_probe = || true;
        let cfg = RunConfig::from_value(json!({
            "pool": true, "pool_grace": "0", "microvmId": "microvm-x", "handoff_prefix": "/p",
        }));
        let watch = app.gate.watch_runs();
        idle_wait_with(app, &cfg, watch).await;
        assert!(
            fake.terminated.lock().unwrap().is_empty(),
            "must stand down"
        );
        assert!(
            fake.gets.lock().unwrap().is_empty(),
            "no claim after stand-down"
        );
    }

    #[tokio::test]
    async fn nothing_suspends_or_reuses_us_terminates() {
        let (app, fake) = test_app("/nonexistent");
        let cfg = RunConfig::from_value(json!({
            "pool": true, "pool_grace": "0", "microvmId": "microvm-x", "handoff_prefix": "/p",
        }));
        let watch = app.gate.watch_runs();
        idle_wait_with(app, &cfg, watch).await;
        // The final claim was attempted, found nothing; no dispatcher_fn in
        // the payload (old dispatcher), so we self-terminated as before.
        assert_eq!(fake.gets.lock().unwrap().as_slice(), ["/p/microvm-x"]);
        assert!(fake.deletes.lock().unwrap().is_empty());
        assert!(fake.invokes.lock().unwrap().is_empty());
        assert_eq!(fake.terminated.lock().unwrap().as_slice(), ["microvm-x"]);
    }

    #[tokio::test]
    async fn grace_expiry_reports_orphan_instead_of_terminating() {
        // With a dispatcher_fn in the payload the expiry path reports
        // reason=orphan and leaves the teardown to the dispatcher.
        let (app, fake) = test_app("/nonexistent");
        let cfg = RunConfig::from_value(json!({
            "pool": true, "pool_grace": "0", "microvmId": "microvm-x",
            "handoff_prefix": "/p", "dispatcher_fn": "disp",
        }));
        let watch = app.gate.watch_runs();
        idle_wait_with(app, &cfg, watch).await;
        let invokes = fake.invokes.lock().unwrap();
        assert_eq!(invokes.len(), 1);
        assert_eq!(invokes[0].0, "disp");
        let event: serde_json::Value = serde_json::from_str(&invokes[0].1).unwrap();
        assert_eq!(
            event,
            json!({"idle": {"microvmId": "microvm-x", "reason": "orphan"}})
        );
        drop(invokes);
        assert!(
            fake.terminated.lock().unwrap().is_empty(),
            "an accepted report must not be followed by a terminate"
        );
    }
}
