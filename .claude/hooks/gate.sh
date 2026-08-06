#!/usr/bin/env bash
# Stop-hook gate. Blocks task completion when the quality gates fail.
# Contract: exit 2 => Claude must keep working; stderr is the fix-it reason.
#           exit 0 => allowed to stop. (exit 1 would NOT block — never use it here.)
set -uo pipefail

input="$(cat)"

# Loop guard: if we're already re-running because a prior Stop-hook blocked,
# don't gate again — let Claude finish its follow-up turn to avoid infinite loops.
if command -v jq >/dev/null 2>&1; then
  active="$(printf '%s' "$input" | jq -r '.stop_hook_active // false')"
else
  active="$(printf '%s' "$input" | grep -q '"stop_hook_active"[[:space:]]*:[[:space:]]*true' && echo true || echo false)"
fi
[ "$active" = "true" ] && exit 0

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0
[ -f Cargo.toml ] || exit 0   # nothing to gate yet

fail=0
report=""
gate() {
  local name="$1"; shift
  local out
  if ! out="$("$@" 2>&1)"; then
    fail=1
    report+=$'\n'"### ${name} failed"$'\n'"${out}"$'\n'
  fi
}

gate "cargo fmt"    cargo fmt --all -- --check
gate "cargo clippy" cargo clippy --all-targets --all-features -- -D warnings

# Tests run in a disposable container, never on the host — see
# `scripts/docker-test.sh`. fmt and clippy stay local on purpose: they are
# fast, they touch nothing outside the source tree, and paying container
# startup for them would slow every turn for no isolation gained.
#
# No host fallback. If the daemon is down this blocks with an explanation
# instead of quietly running the suite locally, because a fallback that fires
# silently is how "tests always run in a container" stops being true.
if docker info >/dev/null 2>&1; then
  gate "tests (docker)" "$PWD/scripts/docker-test.sh"
else
  fail=1
  report+=$'\n'"### tests (docker) could not run"$'\n'"The Docker daemon is unreachable, and tests for this repo only run in a container (scripts/docker-test.sh). Start Docker Desktop and try again."$'\n'
fi

if command -v buf >/dev/null 2>&1 && { [ -f buf.yaml ] || [ -f buf.gen.yaml ]; }; then
  gate "buf lint" buf lint
fi

if [ "$fail" -ne 0 ]; then
  printf 'Quality gates failed — fix before finishing:%s' "$report" >&2
  exit 2
fi
exit 0
