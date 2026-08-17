#!/usr/bin/env bash
# docker-test.sh — run the workspace test suite inside a throwaway container.
#
# The container is disposable: `--rm` plus an EXIT trap that force-removes it,
# so an interrupt or a crashed daemon connection cannot leave one behind. What
# deliberately *survives* is the build cache — the cargo registry and the Linux
# target dir live in named volumes, because a from-scratch rebuild of this
# workspace (ort, bundled SQLite, tonic codegen) is minutes, and a gate that
# costs minutes on every turn is a gate people route around. `--clean` purges
# them when you want the genuinely cold run.
#
# Usage:
#   scripts/docker-test.sh                       # nextest, --all-features, whole workspace
#   scripts/docker-test.sh --no-default-features # extra cargo args (replaces the feature default)
#   scripts/docker-test.sh -p rmail-core         # ...or narrow the run
#   scripts/docker-test.sh --shell               # interactive shell in the same container
#   scripts/docker-test.sh -- <cmd>...           # run an arbitrary command instead
#   scripts/docker-test.sh --clean               # drop the cache volumes and the image
#
# Env:
#   RMAIL_TEST_JOBS  cargo build jobs inside the container (default 3). Bounded
#                    by the daemon's memory, not its cores -- see the comment
#                    above `JOBS` below before raising it.
#
# Exit code is the test command's own, so this drops into any gate unchanged.
set -uo pipefail

IMAGE="rmail-test:local"
VOL_REGISTRY="rmail-test-cargo-registry"
VOL_GIT="rmail-test-cargo-git"

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The target dir is keyed to the checkout, the registry is not. This repo fans
# subagents out into .claude/worktrees/*, and cargo takes an exclusive lock on
# a target dir for the whole build — one shared volume would make concurrent
# worktree runs queue behind each other for minutes at a time. The registry is
# safe to share: its lock is held only for the brief moments cargo is writing a
# downloaded crate, and a shared cache is the entire point.
key="$(printf '%s' "$repo_root" | shasum | cut -c1-12)"
VOL_TARGET="rmail-test-target-${key}"

die() { printf 'docker-test: %s\n' "$*" >&2; exit 1; }

command -v docker >/dev/null 2>&1 || die "docker not found in PATH"
docker info >/dev/null 2>&1 || die "cannot reach the Docker daemon — is Docker Desktop running?"

# ---------------------------------------------------------------------------
# --clean: throw away the persistent half too
# ---------------------------------------------------------------------------
if [ "${1:-}" = "--clean" ]; then
  # Every rmail-test-* volume, not just this checkout's: the per-worktree
  # target volumes outlive the worktrees themselves, so cleaning only the
  # current key would leave the stale ones accumulating forever.
  vols="$(docker volume ls -q --filter "name=^rmail-test-" 2>/dev/null)"
  [ -n "$vols" ] && printf '%s\n' "$vols" | xargs docker volume rm -f >/dev/null 2>&1
  docker image rm -f "$IMAGE" >/dev/null 2>&1
  echo "docker-test: removed ${IMAGE} and cache volumes:"
  printf '%s\n' "${vols:-  (none)}"
  exit 0
fi

# ---------------------------------------------------------------------------
# argument handling
# ---------------------------------------------------------------------------
mode="test"
extra=()
if [ "${1:-}" = "--shell" ]; then
  mode="shell"
elif [ "${1:-}" = "--" ]; then
  mode="raw"; shift; extra=("$@")
  [ "${#extra[@]}" -gt 0 ] || die "--  needs a command to run"
else
  extra=("$@")
fi

# ---------------------------------------------------------------------------
# build the image (cached; a no-op layer check when Dockerfile.test is unchanged)
#
# The Dockerfile goes in on stdin, which makes the build context empty. Passing
# the repo as context instead uploaded 10.3GB — ./target plus .claude/worktrees
# — to the daemon on *every* invocation, ~110s of pure overhead for a build
# whose layers were all cached, and the image COPYs none of it. A
# `Dockerfile.test.dockerignore` would not have helped: this daemon has no
# buildx, and the legacy builder only honours a repo-root `.dockerignore`.
# ---------------------------------------------------------------------------
if ! build_log="$(docker build -t "$IMAGE" - < "$repo_root/Dockerfile.test" 2>&1)"; then
  printf '%s\n' "$build_log" >&2
  die "image build failed"
fi

# ---------------------------------------------------------------------------
# the command to run inside
# ---------------------------------------------------------------------------
case "$mode" in
  shell) cmd=(bash) ;;
  raw)   cmd=("${extra[@]}") ;;
  test)
    cmd=(cargo nextest run --locked --workspace)
    # `--all-features` is only a default. Re-adding it alongside a caller's
    # `--no-default-features` would make cargo reject the invocation outright,
    # and silently overriding a caller's `--features` would run a different
    # suite than they asked for.
    picks_features=0
    for a in ${extra[@]+"${extra[@]}"}; do
      case "$a" in
        --all-features|--no-default-features|--features|--features=*|-F|-F*) picks_features=1 ;;
      esac
    done
    [ "$picks_features" -eq 0 ] && cmd+=(--all-features)
    cmd+=(${extra[@]+"${extra[@]}"})
    ;;
esac

# ---------------------------------------------------------------------------
# run it, and make sure nothing outlives this script
# ---------------------------------------------------------------------------
name="rmail-test-$$-${RANDOM}"
# shellcheck disable=SC2329  # invoked by the `trap` below, which shellcheck
# cannot see. Removing it because it "is never invoked" would leave a
# container running after every interrupted run.
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

# ---------------------------------------------------------------------------
# Build parallelism is bounded by memory here, not by cores
#
# Every integration test in this workspace links the entire dependency tree
# statically -- ~250 rlibs each, and since the OpenPGP work (`rmail-core::crypto`)
# that includes rPGP's crypto tree as well: rsa, the curve25519/p256/p384/p521
# stack, and their bigint backends. A single `cc` invocation for one of those
# binaries peaks in the low gigabytes.
#
# Cargo defaults to one build job per CPU. On this daemon that is 8 jobs against
# a VM with far less than 8x that peak, and the result is the Linux OOM killer
# taking out `ld` -- which surfaces as `collect2: fatal error: ld terminated
# with signal 9` and `rustc ... (signal: 9, SIGKILL: kill)`. Neither message
# says "out of memory", which is what makes this worth a comment rather than a
# number: the failure reads like a broken toolchain, and the last person to hit
# it went looking for a linker bug.
#
# 3 is chosen to fit the default Docker Desktop allocation (~12GB) with room for
# the test processes themselves. Raise it with RMAIL_TEST_JOBS on a machine with
# more memory given to the daemon; the setting only bounds *build* concurrency,
# so test execution parallelism is unaffected.
JOBS="${RMAIL_TEST_JOBS:-3}"

run_args=(
  --rm
  --init
  --name "$name"
  -e "CARGO_BUILD_JOBS=$JOBS"
  -v "$repo_root:/w"
  -v "$VOL_REGISTRY:/usr/local/cargo/registry"
  -v "$VOL_GIT:/usr/local/cargo/git"
  -v "$VOL_TARGET:/target"
  -w /w
)

# The real-model tests decide whether to run by asking whether the weights are
# already cached (see rmail-core/src/embed/local/tests.rs). With no cache in
# here they would all skip and the container would report a green suite that
# never exercised ONNX at all — so the host's cache is mounted at the path the
# default resolution finds. Read-only: a test run may consume the weights, it
# may not rewrite the developer's cache. Nothing is mounted when the host has
# no weights either, which reproduces the documented cold-start behaviour.
host_models="${RMAIL_MODEL_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/rmail/models}"
if [ -d "$host_models" ]; then
  run_args+=(-v "$host_models:/root/.cache/rmail/models:ro")
fi

# A TTY only for an interactive shell, and only when there is one on the host
# to inherit; the Stop gate captures output from a pipe, where -t would inject
# control characters into the failure report. Test/raw runs never get one:
# docker's -t allocates the pty for the container's stdin too, even without
# -i and even with the host's stdin redirected from /dev/null, so a `test`
# run would make `std::io::IsTerminal::is_terminal()` report true inside the
# container with nothing ever attached to answer a prompt — any code under
# test that branches on stdin being a tty (e.g. a confirmation prompt) then
# blocks forever instead of taking its non-interactive path.
if [ "$mode" = "shell" ]; then
  [ -t 1 ] && run_args+=(-t)
  run_args+=(-i)
fi

docker run "${run_args[@]}" "$IMAGE" "${cmd[@]}"
status=$?
exit "$status"
