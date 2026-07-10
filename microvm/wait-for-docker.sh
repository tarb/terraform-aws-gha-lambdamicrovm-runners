#!/usr/bin/env bash
# GitHub Actions runner job-started hook (ACTIONS_RUNNER_HOOK_JOB_STARTED).
#
# Wired per-run, not image-wide: the entrypoint supervisor injects the
# ACTIONS_RUNNER_HOOK_JOB_STARTED env into the runner process only for
# docker-enabled jobs, so non-docker jobs never run this gate at all.
#
# For docker-enabled jobs the supervisor warms up dockerd in the BACKGROUND (in
# parallel with runner registration + GitHub job assignment). This hook runs AFTER
# the runner picks up a job but BEFORE the job's first step, and blocks until
# `docker info` succeeds - so a docker / compose / services step can never race
# the daemon.
#
# Best-effort: after a timeout it proceeds anyway (exit 0), so a job that does not
# use docker is not blocked forever if the daemon genuinely failed - a docker step
# would then surface the real error itself. Exiting non-zero would fail the job,
# which we do not want for non-docker jobs.
set -u

[ "${ENABLE_DOCKER:-true}" = "true" ] || exit 0

budget="${DOCKER_WAIT_TIMEOUT:-150}" # wall-clock seconds; covers dockerd's retry attempts

# The budget is WALL CLOCK (bash's SECONDS), not an iteration count: blocking
# `docker info` calls count against it. Counting iterations let "150s" stretch
# to 4m30s of real time when each probe blocked.
SECONDS=0
while [ "$SECONDS" -lt "$budget" ]; do
  # Bound each probe too: against a stale socket or a wedged daemon,
  # `docker info` has no client-side timeout and can hang forever, which
  # used to stall this hook indefinitely.
  if timeout "${DOCKER_CHECK_TIMEOUT:-5}" docker info >/dev/null 2>&1; then
    exit 0
  fi
  sleep 1
done

echo "wait-for-docker: dockerd not ready after ${SECONDS}s (budget ${budget}s); continuing" >&2
# Make the failure diagnose itself in the job log rather than pointing at a
# file inside a VM that will be gone.
if [ -f /tmp/dockerd.log ]; then
  echo "wait-for-docker: last lines of /tmp/dockerd.log:" >&2
  tail -n 20 /tmp/dockerd.log >&2 || true
fi
exit 0
