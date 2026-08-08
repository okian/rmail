#!/usr/bin/env bash
# Wait until no sibling rmail-test container is running, so a build of this
# worktree's test binaries is not competing for the daemon's 8 GB. Temporary
# scaffolding for the parallel build; not part of the task's deliverable.
set -u
for _ in $(seq 1 90); do
  if [ "$(docker ps --format '{{.Names}}' | grep -c rmail-test)" -eq 0 ]; then
    echo "clear"
    exit 0
  fi
  sleep 20
done
echo "still busy"
exit 1
