#!/usr/bin/env bash
# Retry a container test run until it is not killed by the OOM reaper.
#
# Several sibling agents share one 8 GB Docker daemon in this parallel build,
# and two concurrent Rust link steps do not fit. A SIGKILL (exit 137, or a
# `ld terminated with signal 9`) is contention, not a test result, so it is
# retried; anything else is the answer.
#
# Temporary scaffolding for the parallel build; not part of the deliverable.
set -u
log="$1"
shift

for attempt in $(seq 1 12); do
  # Wait for a quiet window first — starting into a busy daemon just burns
  # a slot for both builds.
  for _ in $(seq 1 30); do
    running=$(docker ps --format '{{.Names}}' | grep -c rmail-test)
    if [ "$running" -le 1 ]; then break; fi
    sleep 20
  done

  # `line-tables-only` debuginfo on the *test* profile only (dependencies
  # keep the dev profile): it cuts the link step's peak memory by more than
  # half without changing a single byte of what is executed, which is what
  # lets this fit alongside a sibling build.
  scripts/docker-test.sh -- env CARGO_BUILD_JOBS=1 \
    CARGO_PROFILE_TEST_DEBUG=line-tables-only "$@" >"$log" 2>&1
  status=$?
  if [ "$status" -ne 137 ] && ! grep -q "signal 9" "$log"; then
    echo "attempt ${attempt}: exit ${status}"
    exit "$status"
  fi
  echo "attempt ${attempt}: killed by the OOM reaper; retrying"
  sleep 30
done
echo "gave up after repeated OOM kills"
exit 137
