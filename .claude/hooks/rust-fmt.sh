#!/usr/bin/env bash
# PostToolUse hook: keep Rust formatting clean cheaply after edits.
# Always non-blocking (exit 0) — formatting is not a gate, it's hygiene.
set -uo pipefail
cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0
[ -f Cargo.toml ] || exit 0
cargo fmt --all >/dev/null 2>&1 || true
exit 0
