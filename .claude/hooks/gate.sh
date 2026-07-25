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

TEST_CMD=(cargo test --all-features --workspace)
command -v cargo-nextest >/dev/null 2>&1 && TEST_CMD=(cargo nextest run --all-features --workspace)

gate "cargo fmt"    cargo fmt --all -- --check
gate "cargo clippy" cargo clippy --all-targets --all-features -- -D warnings
gate "tests"        "${TEST_CMD[@]}"

if command -v buf >/dev/null 2>&1 && { [ -f buf.yaml ] || [ -f buf.gen.yaml ]; }; then
  gate "buf lint" buf lint
fi

if [ "$fail" -ne 0 ]; then
  printf 'Quality gates failed — fix before finishing:%s' "$report" >&2
  exit 2
fi
exit 0
