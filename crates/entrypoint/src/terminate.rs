//! Self-termination: terminate THIS MicroVM now so billing stops
//! immediately. LOUD and retried — a silently failing call leaves the VM
//! billing until max-duration.

use crate::aws::CloudControl;
use crate::logfmt::{log, truncate_chars};
use std::time::Duration;
use types::MicrovmId;

/// Terminate this VM. 3 attempts with a 20 s call timeout and 2/4/6 s
/// backoff; never returns an error — on exhaustion the sweep reaper /
/// max-duration backstop reap the VM.
pub async fn self_terminate(aws: &dyn CloudControl, microvm_id: Option<&MicrovmId>, region: &str) {
    let Some(id) = microvm_id else {
        log("no microvmId in /run payload; relying on max-duration backstop");
        return;
    };
    log(format!(
        "job done - self-terminating microvm {id} (region {region})"
    ));
    for attempt in 1..=3u32 {
        match tokio::time::timeout(Duration::from_secs(20), aws.terminate_microvm(id.as_str()))
            .await
        {
            Ok(Ok(())) => {
                log("self-terminate accepted - teardown imminent");
                return;
            }
            Ok(Err(e)) => log(format!(
                "self-terminate attempt {attempt} raised: {}",
                truncate_chars(&e.to_string(), 300)
            )),
            Err(_) => log(format!(
                "self-terminate attempt {attempt} raised: timed out after 20s"
            )),
        }
        // Back off after every failed attempt, including the last.
        tokio::time::sleep(Duration::from_secs(2 * u64::from(attempt))).await;
    }
    log("self-terminate FAILED after 3 attempts - sweep reaper / max-duration will reap");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aws::testsupport::FakeCloud;

    #[tokio::test(start_paused = true)]
    async fn terminate_self_retries_then_succeeds() {
        let fake = FakeCloud::default();
        *fake.terminate_failures.lock().unwrap() = 2;
        self_terminate(&fake, Some(&MicrovmId::new("microvm-abc123")), "us-east-1").await;
        assert_eq!(
            fake.terminated.lock().unwrap().as_slice(),
            ["microvm-abc123"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn terminate_self_without_id_is_a_noop() {
        let fake = FakeCloud::default();
        self_terminate(&fake, None, "us-east-1").await;
        assert!(fake.terminated.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn terminate_self_gives_up_after_three_attempts() {
        let fake = FakeCloud::default();
        *fake.terminate_failures.lock().unwrap() = 3;
        self_terminate(&fake, Some(&MicrovmId::new("microvm-abc123")), "us-east-1").await;
        assert!(fake.terminated.lock().unwrap().is_empty());
    }
}
