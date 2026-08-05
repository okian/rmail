#!/usr/bin/env bash
# update-buf-baseline.sh — regenerate the committed `buf breaking` baseline.
#
# `proto/buf-baseline.binpb` is a `buf build` image: a frozen snapshot of
# every proto file's compiled descriptor, checked into git and diffed against
# on every CI run (see `.github/workflows/ci.yml`'s "buf breaking" step).
# There's no `main` branch or registry in this repo for `buf breaking
# --against '.git#branch=main'`/`--against-registry` to resolve against, so a
# committed image is the baseline that's actually available — and it has the
# advantage of working offline, identically in CI and on a laptop.
#
# Run this, and commit the result, exactly when a breaking proto change is
# *intentional* (a new `rmail.v2` package served alongside v1, per CLAUDE.md's
# "breaking changes require a new version" rule) — never to silence a
# breaking-change failure on an accidental one. If `buf breaking` just failed
# and the fix is "regenerate the baseline," stop and re-read the diff first.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

command -v buf >/dev/null 2>&1 || { echo "buf not found — install it first (brew install bufbuild/buf/buf)" >&2; exit 1; }

buf build -o proto/buf-baseline.binpb
echo "==> wrote proto/buf-baseline.binpb from the current proto tree"
echo "==> review with 'git diff --stat proto/buf-baseline.binpb', then commit it alongside the intentional breaking change"
