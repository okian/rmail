#!/usr/bin/env bash
# package-macos.sh — build a distributable macOS release bundle for rmaild + mail.
#
# Produces dist/rmail-<version>-macos-<arch>.tar.gz containing:
#   bin/rmaild, bin/mail   - release binaries, ad-hoc codesigned
#   .env.example           - the documented config knobs
#   VERSION                - workspace version + git commit this was built from
#   SHA256SUMS             - checksums of everything above
#
# and a top-level dist/rmail-<version>-macos-<arch>.tar.gz.sha256 next to the
# archive, since a checksum shipped *inside* the archive it verifies is
# useless to a client who has not yet decided to trust the archive.
#
# Usage:
#   scripts/package-macos.sh [--skip-build] [--universal] [--out <dir>] [--version <ver>]
#     --skip-build   don't run `cargo build --release`; use whatever is
#                    already in $CARGO_TARGET_DIR/release (or target/release).
#                    CI uses this: the gate's own "build release" step already
#                    built with the workspace's shared, cached target dir, and
#                    a second `cargo build` here would just redo it.
#     --universal    also build `x86_64-apple-darwin` and fuse the two release
#                    binaries into a universal (arm64+x86_64) binary via
#                    `lipo`. Off by default: it doubles build time and needs
#                    the x86_64 target installed (`rustup target add
#                    x86_64-apple-darwin`), which a fresh macOS toolchain does
#                    not have unless asked for. The CI smoke test does not
#                    pass this — it only proves the packaging step itself
#                    works, not that every architecture combination does.
#     --out <dir>    output directory (default: dist)
#     --version <v>  override the version in the archive name / VERSION file
#                    (default: `[workspace.package].version` from Cargo.toml)
#
# What this deliberately does NOT do: real Apple code signing or
# notarization. Both require a paid Developer ID identity and credentials
# that do not exist in this build environment. The ad-hoc `codesign --sign -`
# below satisfies the local Gatekeeper checks a freshly-built (not
# downloaded, so unquarantined) binary already passes, and nothing more —
# shipping outside a trusted internal channel needs that step added with real
# credentials before this script is otherwise reusable as-is.
set -euo pipefail

# Prints the header comment block (everything between the shebang and the
# `set -euo pipefail` above) with the leading "# " stripped. Reads the file
# top-down and stops at the first line that is neither the shebang nor a
# comment, so it needs no fragile end-pattern to match against — a `grep
# '^#' | sed -n '<n>,/pattern/p'` approach here would either need the pattern
# to appear *inside* the already-filtered comment-only stream (it doesn't:
# `set -euo pipefail` is not a comment line, so `grep '^#'` removes it before
# `sed` ever sees it as an end marker) or hardcode a line number that silently
# drifts the next time a line is added above it.
usage() {
  awk '
    NR == 1 && /^#!/ { next }
    /^#/ { sub(/^# ?/, ""); print; next }
    { exit }
  ' "$0"
}

SKIP_BUILD=0
UNIVERSAL=0
OUT_DIR="dist"
VERSION_OVERRIDE=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    --universal) UNIVERSAL=1; shift ;;
    --out)
      [[ $# -ge 2 ]] || { echo "--out requires a value" >&2; usage >&2; exit 2; }
      OUT_DIR="$2"; shift 2 ;;
    --version)
      [[ $# -ge 2 ]] || { echo "--version requires a value" >&2; usage >&2; exit 2; }
      VERSION_OVERRIDE="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown flag: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# Resolve paths relative to the repo root regardless of where this was
# invoked from, so `scripts/package-macos.sh` works the same from a checkout
# root or a CI working-directory step.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# Defaults to cargo's own default (`target`, relative to the repo root) —
# the right answer for a plain checkout with no other cargo state around.
# Honors `CARGO_TARGET_DIR` when a caller has set one (e.g. pointing at
# wherever a prior build step in the same invocation already placed the
# binaries), but never assumes one: a target directory shared *across*
# concurrent checkouts is not safe to begin with — cargo uplifts final
# binaries to `<target>/release/<name>` by a name-only path, not one keyed by
# source checkout, so two checkouts building the same binary name into a
# shared target directory can overwrite each other's output mid-build. Each
# checkout of this repo should have its own target directory.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

command -v cargo >/dev/null 2>&1 || { echo "cargo not found" >&2; exit 1; }

VERSION="$VERSION_OVERRIDE"
if [[ -z "$VERSION" ]]; then
  # `exit` after the first match means awk itself stops reading and prints
  # exactly one line — no downstream `head -1` needed to discard a second
  # match, and therefore no pipe for `head` to close early, which under
  # `pipefail` could otherwise surface as a spurious SIGPIPE (128+13 = 141)
  # if `[workspace.package]` ever grew a second line matching this pattern.
  # (Not sed's `0,/pat/{ ... }` range-block syntax: it works under GNU sed
  # but not the BSD sed macOS ships, which is exactly the sed this script
  # will actually run under.)
  VERSION="$(awk '/^version = "/ { sub(/^version = "/, ""); sub(/"$/, ""); print; exit }' Cargo.toml)"
fi
[[ -n "$VERSION" ]] || { echo "could not determine version from Cargo.toml; pass --version" >&2; exit 1; }

COMMIT="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
HOST_ARCH_RAW="$(uname -m)"
case "$HOST_ARCH_RAW" in
  arm64|aarch64) HOST_ARCH="arm64" ;;
  x86_64)        HOST_ARCH="x86_64" ;;
  *) echo "unsupported host architecture: $HOST_ARCH_RAW" >&2; exit 1 ;;
esac

build_release() {
  local triple="$1"
  # `--locked`: this script produces the artifact that actually ships, so it
  # builds against the committed `Cargo.lock` or fails — not whatever a
  # looser `Cargo.toml` constraint happens to re-resolve to on the day
  # someone runs it.
  if [[ -n "$triple" ]]; then
    echo "==> building release for $triple"
    cargo build --locked --release --target "$triple" -p rmaild -p rmail-cli
  else
    echo "==> building release for host ($HOST_ARCH)"
    cargo build --locked --release -p rmaild -p rmail-cli
  fi
}

bin_dir_for() {
  local triple="$1"
  if [[ -n "$triple" ]]; then
    printf '%s/%s/release' "$TARGET_DIR" "$triple"
  else
    printf '%s/release' "$TARGET_DIR"
  fi
}

if [[ $UNIVERSAL -eq 1 ]]; then
  ARCH_LABEL="universal"
  if [[ $SKIP_BUILD -eq 0 ]]; then
    rustup target add x86_64-apple-darwin aarch64-apple-darwin >/dev/null 2>&1 || true
    build_release "aarch64-apple-darwin"
    build_release "x86_64-apple-darwin"
  fi
  ARM_DIR="$(bin_dir_for aarch64-apple-darwin)"
  X86_DIR="$(bin_dir_for x86_64-apple-darwin)"
  for bin in rmaild mail; do
    [[ -f "$ARM_DIR/$bin" ]] || { echo "missing $ARM_DIR/$bin (build for aarch64-apple-darwin first)" >&2; exit 1; }
    [[ -f "$X86_DIR/$bin" ]] || { echo "missing $X86_DIR/$bin (build for x86_64-apple-darwin first)" >&2; exit 1; }
  done
  LIPO_SRC_ARM="$ARM_DIR"
  LIPO_SRC_X86="$X86_DIR"
else
  ARCH_LABEL="$HOST_ARCH"
  if [[ $SKIP_BUILD -eq 0 ]]; then
    build_release ""
  fi
  BIN_DIR="$(bin_dir_for "")"
  for bin in rmaild mail; do
    [[ -f "$BIN_DIR/$bin" ]] || { echo "missing $BIN_DIR/$bin — run without --skip-build, or build first" >&2; exit 1; }
  done
fi

NAME="rmail-${VERSION}-macos-${ARCH_LABEL}"
STAGE="$OUT_DIR/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE/bin"

HAVE_CODESIGN=0
command -v codesign >/dev/null 2>&1 && HAVE_CODESIGN=1
[[ $HAVE_CODESIGN -eq 1 ]] || echo "==> warning: codesign not found (not on macOS?) — shipping unsigned binaries" >&2

for bin in rmaild mail; do
  if [[ $UNIVERSAL -eq 1 ]]; then
    lipo -create -output "$STAGE/bin/$bin" "$LIPO_SRC_ARM/$bin" "$LIPO_SRC_X86/$bin"
    echo "==> lipo'd universal $bin ($(lipo -archs "$STAGE/bin/$bin"))"
  else
    cp "$BIN_DIR/$bin" "$STAGE/bin/$bin"
  fi
  chmod 755 "$STAGE/bin/$bin"
  # Ad-hoc signature only — see the header comment on why real Developer ID
  # signing/notarization is out of scope here. Skipped (not swallowed) when
  # `codesign` itself isn't on this machine; a genuine signing failure on a
  # machine that does have it is not swallowed either — a package that
  # silently shipped unsigned because signing broke, while claiming signed
  # binaries in the header comment, is worse than this script failing loudly.
  if [[ $HAVE_CODESIGN -eq 1 ]]; then
    codesign --force --sign - "$STAGE/bin/$bin"
  fi
done

# Prove the binaries in the bundle actually run, not just that `cp`/`lipo`/
# `codesign` exited zero — a corrupt copy or a bad lipo fuse would otherwise
# only surface once someone downloads the archive. Unconditional on
# `--universal`: a fused universal binary still contains the host's own
# slice, so `mail --version` runs it exactly the same either way — skipping
# this check for the one build mode most likely to fuse two binaries
# incorrectly would defeat the point of having it.
#
# `mail --version` is safe to actually execute: clap resolves `--version`
# before touching any gRPC/daemon connection. `rmaild` cannot be smoke-run the
# same way — it takes no CLI arguments at all (see `rmaild/src/main.rs`) and
# starts serving immediately, opening the real database and binding the real
# socket, then blocking indefinitely on a shutdown signal; invoking it here
# would hang this script (and CI) rather than test anything. `file` confirms
# it is a valid, correctly-architected Mach-O executable without starting it;
# `rmaild`'s own integration tests (`rmaild/tests/`) are what actually boot it.
"$STAGE/bin/mail" --version >/dev/null
file "$STAGE/bin/rmaild" | grep -q "Mach-O" || { echo "packaged rmaild is not a valid Mach-O binary" >&2; exit 1; }

if [[ $UNIVERSAL -eq 1 ]]; then
  # A lipo fuse that silently dropped a slice (a stale single-arch binary
  # handed to `-create`, a wrong source path) still passes the plain `file`
  # check above — it just describes whichever one slice is actually there.
  # This is the check that would catch that.
  for bin in rmaild mail; do
    archs="$(lipo -archs "$STAGE/bin/$bin")"
    case "$archs" in
      *arm64*x86_64*|*x86_64*arm64*) : ;;
      *) echo "packaged $bin is not a universal (arm64+x86_64) binary: got '$archs'" >&2; exit 1 ;;
    esac
  done
fi

cp .env.example "$STAGE/.env.example"
{
  echo "rmail $VERSION"
  echo "commit: $COMMIT"
  echo "built:  $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "target: $ARCH_LABEL"
} > "$STAGE/VERSION"

( cd "$STAGE" && shasum -a 256 bin/rmaild bin/mail .env.example VERSION > SHA256SUMS )

mkdir -p "$OUT_DIR"
TARBALL="$OUT_DIR/$NAME.tar.gz"
tar -czf "$TARBALL" -C "$OUT_DIR" "$NAME"
( cd "$OUT_DIR" && shasum -a 256 "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )

echo "==> packaged $TARBALL"
echo "==> $(cat "$OUT_DIR/$NAME.tar.gz.sha256")"
