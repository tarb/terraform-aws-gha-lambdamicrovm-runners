//! Idle reporting: tell the dispatcher this VM is done instead of tearing it
//! down from inside.
//!
//! In-guest `TerminateMicrovm` ALWAYS fails when the VPC routes the Lambda
//! API through a PrivateLink interface endpoint (the MicroVMs sub-API
//! rejects it with `AccessDeniedException` "PrivateLink is not yet
//! supported") — but a standard Lambda `Invoke` works over PrivateLink. So
//! the VM reports its idleness to the dispatcher, which suspends (pooling
//! it) or terminates it from the control plane, where everything works.
//! In-VM self-terminate remains the fallback when reporting is impossible
//! or fails outright.

use crate::aws::CloudControl;
use crate::logfmt::{log, truncate_chars};
use crate::payload::RunConfig;
use crate::pool::{Clock, RealClock};
use crate::terminate;
use std::time::Duration;
use types::{IdleEvent, IdleReason, IdleReport};

/// How one idle report ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportOutcome {
    /// An attempt was accepted — the dispatcher owns the teardown (or
    /// suspend) now.
    Accepted,
    /// No attempt was accepted (or none was possible): the caller's fallback
    /// (self-terminate) applies.
    Failed,
    /// A suspend/resume landed MID-report: the failure is an artifact of the
    /// freeze, and being resumed means a new run (a /run or mailbox claim)
    /// owns this VM now. No retry, no fallback — stand down entirely.
    Interrupted,
}

/// Report this VM idle to the dispatcher named in the run payload. Two
/// attempts (10 s call timeout, 2 s backoff), LOUD either way. Returns
/// [`ReportOutcome::Failed`] without any attempt when the payload carries no
/// `dispatcher_fn` (old dispatcher) or no id — behavior is then exactly the
/// pre-report world.
pub async fn report_idle(
    aws: &dyn CloudControl,
    cfg: &RunConfig,
    reason: IdleReason,
) -> ReportOutcome {
    report_idle_with(aws, cfg, reason, &RealClock::new()).await
}

async fn report_idle_with(
    aws: &dyn CloudControl,
    cfg: &RunConfig,
    reason: IdleReason,
    clock: &dyn Clock,
) -> ReportOutcome {
    let Some(function) = cfg.dispatcher_fn.as_deref() else {
        skip_log_once("no dispatcher_fn in /run payload - skipping idle reports");
        return ReportOutcome::Failed;
    };
    let Some(id) = cfg.microvm_id.as_ref() else {
        skip_log_once("no microvmId in /run payload - skipping idle reports");
        return ReportOutcome::Failed;
    };
    let event = IdleEvent {
        idle: IdleReport {
            microvm_id: id.as_str().to_string(),
            reason,
            repo: repo_hint(cfg),
        },
    };
    let payload = serde_json::to_string(&event).expect("IdleEvent always serializes");
    for attempt in 1..=2u32 {
        // Snapshot BOTH clocks per attempt: a job-complete report races the
        // dispatcher's suspend of THIS VM (triggered by the completed
        // webhook, or by this report's own first delivery when only the
        // response got lost) — the freeze then lands mid-invoke and the
        // attempt "fails" by timeout after the thaw.
        let (mono, wall) = (clock.monotonic(), clock.wall());
        match tokio::time::timeout(
            Duration::from_secs(10),
            aws.invoke_function(function, &payload),
        )
        .await
        {
            Ok(Ok(())) => {
                log(format!("idle report accepted (reason {reason})"));
                return ReportOutcome::Accepted;
            }
            Ok(Err(e)) => log(format!(
                "idle report attempt {attempt} failed: {}",
                truncate_chars(&e.to_string(), 300)
            )),
            Err(_) => log(format!(
                "idle report attempt {attempt} failed: timed out after 10s"
            )),
        }
        // Wall time leaping past monotonic progress across the attempt means
        // we were suspended and resumed while it was in flight.
        if (clock.wall() - wall) - (clock.monotonic() - mono) > 60.0 {
            log("idle report interrupted by suspend/resume - standing down");
            return ReportOutcome::Interrupted;
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    ReportOutcome::Failed
}

/// The `owner/repo` busy-check hint for the report, from the run payload's
/// `github_url`. Absent when the URL is absent or anything but a plain
/// `https://github.com/{owner}/{repo}` — the dispatcher then falls back to
/// its bounded fleet-wide scan.
fn repo_hint(cfg: &RunConfig) -> Option<String> {
    let path = cfg
        .github_url
        .as_deref()?
        .strip_prefix("https://github.com/")?
        .trim_end_matches('/');
    let (owner, repo) = path.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/'))
        .then(|| format!("{owner}/{repo}"))
}

/// The teardown chain everywhere the VM used to self-terminate directly:
/// report idle first (the dispatcher suspends or terminates from the control
/// plane); only on total report failure fall back to the in-VM
/// self-terminate (which itself falls back to the sweep reaper /
/// max-duration backstop). An [`Interrupted`](ReportOutcome::Interrupted)
/// report stands down instead — the resume's new run owns the VM.
pub async fn report_idle_or_terminate(
    aws: &dyn CloudControl,
    cfg: &RunConfig,
    region: &str,
    reason: IdleReason,
) {
    report_idle_or_terminate_with(aws, cfg, region, reason, &RealClock::new()).await;
}

async fn report_idle_or_terminate_with(
    aws: &dyn CloudControl,
    cfg: &RunConfig,
    region: &str,
    reason: IdleReason,
    clock: &dyn Clock,
) {
    match report_idle_with(aws, cfg, reason, clock).await {
        ReportOutcome::Accepted | ReportOutcome::Interrupted => {}
        ReportOutcome::Failed => {
            terminate::self_terminate(aws, cfg.microvm_id.as_ref(), region).await;
        }
    }
}

/// The skip cases fire on every teardown of every run; one line per process
/// is enough.
fn skip_log_once(msg: &str) {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| log(msg));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::testsupport::FakeCloud;
    use serde_json::json;
    use std::sync::Mutex;

    fn cfg_with_dispatcher() -> RunConfig {
        RunConfig::from_value(json!({
            "microvmId": "microvm-abc123",
            "dispatcher_fn": "gha-microvm-dispatcher",
        }))
    }

    /// Injectable clock scripted per READ (like the IdleWait tests' mock,
    /// but sequenced): each `monotonic()`/`wall()` call pops the next
    /// scripted value; the last repeats. A suspend mid-attempt is scripted
    /// as a wall value that leaps between the pre- and post-attempt reads.
    struct ScriptClock {
        mono: Mutex<Vec<f64>>,
        wall: Mutex<Vec<f64>>,
    }

    impl ScriptClock {
        fn new(mono: &[f64], wall: &[f64]) -> Self {
            Self {
                mono: Mutex::new(mono.to_vec()),
                wall: Mutex::new(wall.to_vec()),
            }
        }

        fn next(queue: &Mutex<Vec<f64>>) -> f64 {
            let mut q = queue.lock().unwrap();
            if q.len() > 1 { q.remove(0) } else { q[0] }
        }
    }

    impl Clock for ScriptClock {
        fn monotonic(&self) -> f64 {
            Self::next(&self.mono)
        }

        fn wall(&self) -> f64 {
            Self::next(&self.wall)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn accepted_report_means_no_terminate_call() {
        let fake = FakeCloud::default();
        report_idle_or_terminate(
            &fake,
            &cfg_with_dispatcher(),
            "us-east-1",
            IdleReason::JobComplete,
        )
        .await;
        let invokes = fake.invokes.lock().unwrap();
        assert_eq!(invokes.len(), 1);
        assert_eq!(invokes[0].0, "gha-microvm-dispatcher");
        let event: serde_json::Value = serde_json::from_str(&invokes[0].1).unwrap();
        assert_eq!(
            event,
            json!({"idle": {"microvmId": "microvm-abc123", "reason": "job-complete"}})
        );
        drop(invokes);
        assert!(
            fake.terminated.lock().unwrap().is_empty(),
            "the dispatcher owns the teardown after an accepted report"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn report_retries_once_then_succeeds() {
        let fake = FakeCloud::default();
        *fake.invoke_failures.lock().unwrap() = 1;
        assert_eq!(
            report_idle(&fake, &cfg_with_dispatcher(), IdleReason::Orphan).await,
            ReportOutcome::Accepted
        );
        assert_eq!(fake.invokes.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn suspend_resume_mid_report_stands_down_without_retry() {
        // The freeze/retry race: attempt 1 fails, and across it the wall
        // clock leapt an hour past monotonic progress — we were suspended
        // and resumed mid-report. The incoming run owns the VM: no retry,
        // and (below) no terminate fallback.
        let fake = FakeCloud::default();
        *fake.invoke_failures.lock().unwrap() = 2; // every attempt would fail
        // Reads per attempt: mono(pre), wall(pre), wall(post), mono(post).
        let clock = ScriptClock::new(&[0.0, 5.0], &[0.0, 3600.0]);
        let out = report_idle_with(
            &fake,
            &cfg_with_dispatcher(),
            IdleReason::JobComplete,
            &clock,
        )
        .await;
        assert_eq!(out, ReportOutcome::Interrupted);
        assert_eq!(
            *fake.invoke_failures.lock().unwrap(),
            1,
            "exactly ONE attempt was made - no retry after the jump"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn interrupted_report_never_falls_back_to_terminate() {
        let fake = FakeCloud::default();
        *fake.invoke_failures.lock().unwrap() = 2;
        let clock = ScriptClock::new(&[0.0, 5.0], &[0.0, 3600.0]);
        report_idle_or_terminate_with(
            &fake,
            &cfg_with_dispatcher(),
            "us-east-1",
            IdleReason::JobComplete,
            &clock,
        )
        .await;
        assert!(
            fake.terminated.lock().unwrap().is_empty(),
            "standing down means NO terminate - the resume's run owns the VM"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn small_clock_drift_is_not_a_suspend_and_still_retries() {
        // 60 s of drift exactly is NOT a jump (strictly greater, matching
        // IdleWait::observe); the retry proceeds and succeeds.
        let fake = FakeCloud::default();
        *fake.invoke_failures.lock().unwrap() = 1;
        let clock = ScriptClock::new(&[0.0, 5.0], &[0.0, 65.0]);
        let out = report_idle_with(
            &fake,
            &cfg_with_dispatcher(),
            IdleReason::JobComplete,
            &clock,
        )
        .await;
        assert_eq!(out, ReportOutcome::Accepted);
        assert_eq!(fake.invokes.lock().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn total_report_failure_falls_back_to_self_terminate() {
        let fake = FakeCloud::default();
        *fake.invoke_failures.lock().unwrap() = 2; // both attempts fail
        report_idle_or_terminate(
            &fake,
            &cfg_with_dispatcher(),
            "us-east-1",
            IdleReason::Orphan,
        )
        .await;
        assert!(fake.invokes.lock().unwrap().is_empty());
        assert_eq!(
            fake.terminated.lock().unwrap().as_slice(),
            ["microvm-abc123"],
            "terminate_self must be attempted after two failed reports"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn missing_dispatcher_fn_skips_reporting_and_terminates_as_before() {
        // Old-dispatcher payload: no dispatcher_fn — no invoke is ever tried
        // and the pre-report behavior (self-terminate) is preserved.
        let fake = FakeCloud::default();
        let cfg = RunConfig::from_value(json!({"microvmId": "microvm-abc123"}));
        report_idle_or_terminate(&fake, &cfg, "us-east-1", IdleReason::JobComplete).await;
        assert!(fake.invokes.lock().unwrap().is_empty());
        assert_eq!(
            fake.terminated.lock().unwrap().as_slice(),
            ["microvm-abc123"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn orphan_reason_rides_the_wire() {
        let fake = FakeCloud::default();
        assert_eq!(
            report_idle(&fake, &cfg_with_dispatcher(), IdleReason::Orphan).await,
            ReportOutcome::Accepted
        );
        let invokes = fake.invokes.lock().unwrap();
        let event: serde_json::Value = serde_json::from_str(&invokes[0].1).unwrap();
        assert_eq!(event["idle"]["reason"], "orphan");
    }

    #[tokio::test(start_paused = true)]
    async fn repo_hint_from_github_url_rides_the_wire() {
        let fake = FakeCloud::default();
        let cfg = RunConfig::from_value(json!({
            "microvmId": "microvm-abc123",
            "dispatcher_fn": "disp",
            "github_url": "https://github.com/octo/repo",
        }));
        report_idle(&fake, &cfg, IdleReason::JobComplete).await;
        let invokes = fake.invokes.lock().unwrap();
        let event: serde_json::Value = serde_json::from_str(&invokes[0].1).unwrap();
        assert_eq!(event["idle"]["repo"], "octo/repo");
    }

    #[test]
    fn repo_hint_rejects_absent_or_odd_urls() {
        let hint = |url: serde_json::Value| {
            let mut payload = json!({"microvmId": "m"});
            if !url.is_null() {
                payload["github_url"] = url;
            }
            repo_hint(&RunConfig::from_value(payload))
        };
        // The one accepted shape, trailing slash tolerated.
        assert_eq!(
            hint(json!("https://github.com/o/r")).as_deref(),
            Some("o/r")
        );
        assert_eq!(
            hint(json!("https://github.com/o/r/")).as_deref(),
            Some("o/r")
        );
        // Absent or odd: no hint - the dispatcher scans instead.
        assert_eq!(hint(serde_json::Value::Null), None);
        assert_eq!(hint(json!("https://github.com/orgonly")), None);
        assert_eq!(hint(json!("https://github.com/o/r/extra")), None);
        assert_eq!(hint(json!("https://github.com//r")), None);
        assert_eq!(hint(json!("https://ghe.example.com/o/r")), None);
        assert_eq!(hint(json!("http://github.com/o/r")), None);
    }
}
