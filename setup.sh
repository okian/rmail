#!/usr/bin/env bash
# setup.sh — bootstrap the Rust/gRPC build-loop harness into the current repo.
# Self-contained: writes .claude/ + CLAUDE.md, wires the hooks, checks the
# toolchain the gate depends on, and commits the harness. It does NOT run
# /prd, /tasks, or /loop — those are interactive and gated on your review.
#
# Usage:
#   ./setup.sh [--force] [--with-cargo-tools] [--no-commit] [--no-pre]
#     --force             overwrite existing harness files
#     --with-cargo-tools  also `cargo install` nextest / deny / audit (slow, network)
#     --no-commit         don't create the harness git commit
#     --no-pre            don't create a pre.md template
set -uo pipefail

FORCE=0; WITH_TOOLS=0; DO_COMMIT=1; DO_PRE=1
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    --with-cargo-tools) WITH_TOOLS=1 ;;
    --no-commit) DO_COMMIT=0 ;;
    --no-pre) DO_PRE=0 ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

c_g="\033[32m"; c_y="\033[33m"; c_r="\033[31m"; c_0="\033[0m"
info(){ printf "${c_g}✓${c_0} %s\n" "$*"; }
warn(){ printf "${c_y}!${c_0} %s\n" "$*"; }
err(){  printf "${c_r}✗${c_0} %s\n" "$*" >&2; }

# w <path> : write stdin to <path>, skipping if it exists (unless --force).
# Always consumes stdin so heredocs stay balanced either way.
w(){
  local path="$1"
  if [[ -e "$path" && $FORCE -ne 1 ]]; then warn "skip (exists): $path"; cat >/dev/null; return; fi
  mkdir -p "$(dirname "$path")"; cat > "$path"; info "wrote $path"
}

# ---------------------------------------------------------------------------
# 0. sanity
# ---------------------------------------------------------------------------
command -v git >/dev/null 2>&1 || { err "git not found — install git first."; exit 1; }
if ! command -v cargo >/dev/null 2>&1; then
  warn "cargo not found. The Stop gate needs a Rust toolchain — install via https://rustup.rs before running /loop."
fi

# ---------------------------------------------------------------------------
# 1. toolchain components the gate shells out to
# ---------------------------------------------------------------------------
if command -v rustup >/dev/null 2>&1; then
  rustup component add rustfmt clippy >/dev/null 2>&1 && info "rustfmt + clippy present" \
    || warn "could not add rustfmt/clippy (offline?) — ensure they're installed."
else
  warn "rustup not found — make sure rustfmt and clippy are available for the gate."
fi
if [[ $WITH_TOOLS -eq 1 ]] && command -v cargo >/dev/null 2>&1; then
  for t in cargo-nextest cargo-deny cargo-audit; do
    if command -v "$t" >/dev/null 2>&1; then info "$t present"; else
      warn "installing $t (this can take a while)..."; cargo install "$t" >/dev/null 2>&1 \
        && info "installed $t" || warn "failed to install $t — skipping (optional)."
    fi
  done
fi

# ---------------------------------------------------------------------------
# 2. write the harness
# ---------------------------------------------------------------------------
w CLAUDE.md <<'CLAUDE_MD'
# Project constitution

Production-grade Rust gRPC service. This file is read every session — keep it lean and authoritative.

## Toolchain (source of truth for gates)
- Format: `cargo fmt --all`  /  check with `cargo fmt --all -- --check`
- Lint: `cargo clippy --all-targets --all-features -- -D warnings`
- Test: `cargo test --all-features --workspace` (prefer `cargo nextest run` if available)
- Build: `cargo build --release`
- Protos: `buf lint` and `buf breaking` if `buf.yaml` exists; otherwise protos build via `tonic-build` in `build.rs`
- Supply chain (final gate): `cargo deny check` and/or `cargo audit`

The `Stop` hook runs the fmt/clippy/test gate and blocks completion on failure. Never declare a task done on a red gate.

## Non-negotiables
- No `unwrap()`, `expect()`, `panic!`, or `todo!()` in non-test code. Handle every error.
- Library/domain errors use `thiserror`; map to `tonic::Status` only at the gRPC boundary with correct codes. `anyhow` allowed at binary top level only.
- Async on `tokio`; never block the runtime. Honor deadlines/cancellation — propagate the request's cancellation token, don't leak tasks.
- Instrument with `tracing` (spans + structured fields); no `println!` for logs.
- Config via env (`figment`/`config`), never hardcoded secrets. Maintain `.env.example`.
- Every behavior is covered by a test that actually runs. Cover error/`Status` paths, not just the happy path.
- Small, reviewable commits (Conventional Commits). One coherent commit per task. Never commit a broken build.

## gRPC conventions
- Protos in `proto/`, versioned package names (`pkg.v1`). Breaking changes require a new version.
- Every service exposes gRPC health (`tonic-health`) and reflection (`tonic-reflection`).
- Cross-cutting concerns (auth, request context, timeouts) via interceptors/`tower` layers, not per-handler code.
- Integration tests run against an in-process `tonic` server, not mocks of the transport.

## Workspace shape
Cargo workspace: a generated-proto crate, a domain/core crate, and the service binary crate. Keep transport, domain, and wiring separated.

## Model routing (cost/speed policy — respect per job)
- **Planning/PRD/architecture** → Opus (high leverage, low volume). Don't route down.
- **Task decomposition & implementation** → Sonnet (the default; the token-heavy work).
- **Mechanical churn** (scaffolding, file moves, formatting, commit messages, updating `tasks.md` checkboxes) → delegate to the `scaffold` subagent (Haiku).
- **Review at task boundaries** → invoke the `reviewer` subagent (Opus) before checking a task done.
- Push verification into the hooks/gates (free, deterministic) rather than asking a model to self-grade.

## State
- `pre.md` = raw human input. `prd.md` = definitive scope. `tasks.md` = definitive progress tracker, always current — it's how a fresh `/loop` resumes.
CLAUDE_MD

w .claude/settings.json <<'SETTINGS_JSON'
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit",
        "hooks": [
          { "type": "command", "command": "$CLAUDE_PROJECT_DIR/.claude/hooks/rust-fmt.sh" }
        ]
      }
    ],
    "Stop": [
      {
        "matcher": "*",
        "hooks": [
          { "type": "command", "command": "$CLAUDE_PROJECT_DIR/.claude/hooks/gate.sh" }
        ]
      }
    ]
  }
}
SETTINGS_JSON

w .claude/commands/prd.md <<'PRD_CMD'
---
description: Turn pre.md into a rigorous PRD + architecture for the Rust/gRPC service.
argument-hint: "[--auto] [--source <file>]"
model: opus
---

# /prd — spec + architecture (Opus)

You are a principal engineer scoping a production Rust gRPC service. Reason hard; this document determines everything downstream. Source: `--source <file>` or default `pre.md`. If it's missing, stop and say so.

Produce `prd.md` containing:
- Problem statement, goals, explicit **non-goals**.
- Primary users / use cases.
- Functional requirements (numbered, testable).
- Non-functional requirements: latency/throughput targets, reliability, security, observability, scalability.
- **Architecture**: crate/workspace layout; the gRPC **service + method contracts** and the proto package/versioning plan; the error taxonomy and how domain errors map to `tonic::Status` codes; interceptor/`tower` layers (auth, timeouts, tracing); health + reflection; config/secrets strategy; deployment shape. One-line rationale per non-obvious choice.
- Data model / external dependencies.
- Success criteria and acceptance tests.
- Assumptions (each open question resolved by a stated assumption).

If `pre.md` has blocking ambiguities: unless `--auto`, ask up to 5 sharp questions in one batch, then proceed. With `--auto`, choose sane defaults and record them under Assumptions.

Write `prd.md`, commit it (`docs: add PRD and architecture`), then print a short summary and tell the user to review it and run `/tasks`. Do not write code in this phase.
PRD_CMD

w .claude/commands/tasks.md <<'TASKS_CMD'
---
description: Decompose prd.md into an ordered, parseable tasks.md work breakdown.
argument-hint: "[--auto]"
model: sonnet
---

# /tasks — work breakdown (Sonnet)

Read `prd.md` (stop if absent). Decompose it into `./tasks.md`. Rules:
- Each task is small and independently shippable (a few hours max).
- Order by dependency. The **first task** scaffolds the workspace and toolchain so everything after it is verifiable: cargo workspace + crates, `rustfmt.toml`, `clippy` config, `tonic-build`/`build.rs`, `proto/` with a versioned package, `tonic-health` + `tonic-reflection`, a test harness (prefer `nextest`), a CI workflow, and `.env.example`.
- Every task carries acceptance criteria and the exact verify commands.
- Mark tasks that are safe to build in parallel (no shared files, no `depends-on` between them) with `parallel-safe: yes` so a future wave run can fan them out.

Use exactly this format so `/loop` can parse and update it:

```
## <ID>. <title>
- [ ] status
- **depends-on:** <IDs or none>
- **parallel-safe:** <yes|no>
- **acceptance:**
  - <criterion>
- **verify:** <commands that must pass, e.g. `cargo test -p my-svc`, `cargo clippy -- -D warnings`>
```

Delegate the mechanical writing of the file to the `scaffold` subagent if helpful. Commit (`docs: add task breakdown`), summarize, and tell the user to run `/loop`.
TASKS_CMD

w .claude/commands/loop.md <<'LOOP_CMD'
---
description: Implement the next unfinished task end-to-end (code, tests, docs), review it, commit it. Resumable.
argument-hint: "[--all]"
model: sonnet
---

# /loop — implementation loop (Sonnet)

You implement tasks from `tasks.md` at production grade. You are **resumable**: read state and continue from the first unchecked task. Never redo completed work. If context runs low, commit progress and stop — the next `/loop` resumes.

By default, do **one** task and stop. With `--all`, continue through tasks until blocked or done.

## Orient
Read `CLAUDE.md`, `prd.md`, and `tasks.md`. If `prd.md` is missing → tell the user to run `/prd`. If `tasks.md` is missing → `/tasks`.

## Per task
1. Pick the first unchecked task whose dependencies are all done. Mark it in-progress in `tasks.md`.
2. Implement it **completely** — production Rust only. No stubs, no `TODO`, no `unwrap()/expect()/panic!` in non-test code. Map domain errors to `tonic::Status` at the boundary. Honor deadlines/cancellation. Instrument with `tracing`.
3. Write tests proving the acceptance criteria — unit plus integration against an in-process `tonic` server where the task touches a service. Cover error/`Status` paths.
4. Run the task's `verify` commands and the full gate: `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, tests, build. Fix until green. (The `Stop` hook enforces this regardless.)
5. Update affected docs: `README`, rustdoc on public items, proto comments, `.env.example`.
6. **Invoke the `reviewer` subagent (Opus)** on the diff. Address its findings before proceeding.
7. Delegate mechanical steps — commit-message wording, checking the `tasks.md` box, file moves — to the `scaffold` subagent (Haiku).
8. Commit with a Conventional Commit scoped to the task. Check the task's box in `tasks.md` and commit that.

If a task shows the plan is wrong, update `prd.md`/`tasks.md`, commit that, then continue.

## When all tasks are checked
Run the full pipeline clean (fmt, clippy, test with coverage, `cargo build --release`, `cargo deny`/`audit`, `buf breaking` if applicable). Confirm no leftover `TODO`/`unwrap` in prod paths. Print a completion report: what shipped, how to run it, and test summary.
LOOP_CMD

w .claude/agents/reviewer.md <<'REVIEWER'
---
name: reviewer
description: Independent senior review of Rust/gRPC changes at task boundaries. Use proactively before marking a task done.
model: opus
tools: Read, Grep, Glob, Bash
---

You are a senior Rust and distributed-systems reviewer. You did not write this code — your job is to find what the implementer missed. Review the current diff (use `git diff` and read the touched files) against these axes and report concrete, actionable findings only:

- **Correctness & error handling**: any `unwrap()/expect()/panic!/todo!` in non-test code; errors swallowed or mismapped; wrong `tonic::Status` codes at the boundary.
- **Concurrency**: blocking calls on the async runtime; unbounded tasks/channels; missing deadline/cancellation propagation; potential deadlocks.
- **gRPC/proto**: backward-incompatible proto changes without a version bump; missing health/reflection wiring; interceptor logic that belongs in a layer.
- **Tests**: acceptance criteria not actually proven; error/`Status` paths untested; integration test mocks the transport instead of using an in-process server.
- **Security**: secrets in code/logs; missing input validation; unsafe blocks without justification.
- **Observability**: missing `tracing` spans/fields on the request path.

You are read-only: you may run `git diff`, `cargo clippy`, and `cargo test` to verify, but do not edit files. Return a short verdict (APPROVE / CHANGES REQUIRED) and a prioritized list. If nothing is wrong, say so plainly — don't invent issues.
REVIEWER

w .claude/agents/scaffold.md <<'SCAFFOLD'
---
name: scaffold
description: Mechanical, non-reasoning work — scaffolding, file moves, formatting, commit messages, updating tasks.md checkboxes. Use for routine churn to keep it off the expensive models.
model: haiku
tools: Read, Write, Edit, Bash
---

You handle mechanical work quickly and exactly. No architecture or design decisions — if a task requires judgment, say so and hand it back.

Typical jobs: create boilerplate files and module stubs from an explicit spec; move/rename files and fix imports; run `cargo fmt`; write Conventional Commit messages for a given diff; flip a task's checkbox and status in `tasks.md`; generate `.env.example` entries from config structs.

Follow `CLAUDE.md`. Never introduce `unwrap()/expect()` in non-test code even in boilerplate. Do exactly what's asked, nothing more.
SCAFFOLD

w .claude/hooks/gate.sh <<'GATE_SH'
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
GATE_SH

w .claude/hooks/rust-fmt.sh <<'FMT_SH'
#!/usr/bin/env bash
# PostToolUse hook: keep Rust formatting clean cheaply after edits.
# Always non-blocking (exit 0) — formatting is not a gate, it's hygiene.
set -uo pipefail
cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0
[ -f Cargo.toml ] || exit 0
cargo fmt --all >/dev/null 2>&1 || true
exit 0
FMT_SH

chmod +x .claude/hooks/*.sh && info "hooks marked executable"

# ---------------------------------------------------------------------------
# 3. pre.md template (only if absent)
# ---------------------------------------------------------------------------
if [[ $DO_PRE -eq 1 ]]; then
  w pre.md <<'PRE_MD'
# pre.md — raw spec (edit me, then run /prd)

## What are we building?
<one paragraph: the service and the problem it solves>

## gRPC surface
- Service: <Name>
  - <Method>(<Request>) -> <Response>: <what it does>

## Constraints & non-functional
- Latency/throughput targets:
- Auth:
- Persistence / external deps:
- Deployment target:

## Out of scope
-

## Open questions
-
PRE_MD
fi

# ---------------------------------------------------------------------------
# 4. git
# ---------------------------------------------------------------------------
if [[ ! -d .git ]]; then
  git init -q && info "git init"
fi
if [[ $DO_COMMIT -eq 1 ]]; then
  git add .claude CLAUDE.md 2>/dev/null || true
  if ! git diff --cached --quiet 2>/dev/null; then
    git commit -q -m "chore: add Rust/gRPC build-loop harness" && info "committed harness" \
      || warn "commit skipped — set git user.name/email, then: git add .claude CLAUDE.md && git commit"
  else
    warn "nothing new to commit (harness already tracked)"
  fi
fi

# ---------------------------------------------------------------------------
# 5. verify what we wrote
# ---------------------------------------------------------------------------
ok=1
bash -n .claude/hooks/gate.sh     || { err "gate.sh syntax error"; ok=0; }
bash -n .claude/hooks/rust-fmt.sh || { err "rust-fmt.sh syntax error"; ok=0; }
if command -v python3 >/dev/null 2>&1; then
  python3 -c "import json;json.load(open('.claude/settings.json'))" 2>/dev/null \
    && info "settings.json valid" || { err "settings.json invalid"; ok=0; }
fi
[[ $ok -eq 1 ]] && info "harness verified"

# ---------------------------------------------------------------------------
# 6. next steps (the interactive part — deliberately not scripted)
# ---------------------------------------------------------------------------
cat <<'NEXT'

──────────────────────────────────────────────
Setup done. The rest is interactive (and gated on your review):

  1. Edit  pre.md  with your real spec.
  2. Open this repo in Claude Code. Run  /hooks  to confirm the
     Stop + PostToolUse hooks registered, and  /doctor  for anything off.
  3. /prd     → Opus writes prd.md + architecture.  REVIEW & edit it.
  4. /tasks   → Sonnet writes tasks.md.             REVIEW & reorder it.
  5. /loop    → runs ONE task as a smoke test. Watch the gate + reviewer fire.
  6. /loop --all → let it work down the list, committing per task.

Notes:
  • The Stop gate runs the full test suite on every turn-end in this repo.
    If that's slow, trim it to fmt+clippy and push tests to CI, or
    re-run with --with-cargo-tools to get cargo-nextest.
  • Enable prompt caching to avoid re-paying for CLAUDE.md/prd.md/tasks.md
    on every loop step.
──────────────────────────────────────────────
NEXT
