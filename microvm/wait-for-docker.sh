#!/usr/bin/env bash
# GitHub Actions runner job-started hook (ACTIONS_RUNNER_HOOK_JOB_STARTED).
#
# The entrypoint supervisor warms up dockerd in the BACKGROUND (in parallel with runner
# registration + GitHub job assignment). This hook runs AFTER the runner picks up
# a job but BEFORE the job's first step, and blocks until `docker info` succeeds -
# so a docker / compose / services step can never race the daemon.
#
# Best-effort: after a timeout it proceeds anyway (exit 0), so a job that does not
# use docker is not blocked forever if the daemon genuinely failed - a docker step
# would then surface the real error itself. Exiting non-zero would fail the job,
# which we do not want for non-docker jobs.
set -u

[ "${ENABLE_DOCKER:-true}" = "true" ] || exit 0

timeout="${DOCKER_WAIT_TIMEOUT:-150}" # cover dockerd's retry attempts
waited=0
while [ "$waited" -lt "$timeout" ]; do
  if docker info >/dev/null 2>&1; then
    exit 0
  fi
  sleep 1
  waited=$((waited + 1))
done

echo "wait-for-docker: dockerd not ready after ${timeout}s; continuing (see /tmp/dockerd.log)" >&2
exit 0
