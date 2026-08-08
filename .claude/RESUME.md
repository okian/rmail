# Resume state — parallel build

`tasks.md` is the definitive progress tracker. This file records only what it
does not carry: which worktree branch holds unfinished work, and the
orchestration rules that were learned the hard way.

## Merged and checked off

**53 of 86 done, 33 remaining.** `tasks.md` is authoritative — count with
`grep -c '^- \[ \]' tasks.md`. Every task is verified on the *combined* tree
after merge, never on the agent's own report: currently **2101/2101** tests,
with clippy, `buf lint`, `cargo deny`, gitleaks and typos all clean.

Four defects in already-merged work have been found by the post-merge
full-suite run or by orchestrator review rather than by the task that
introduced them: a hook-dispatcher boot race, a budget enforcer that did not
bound in-flight batches, a misspelled test name, and — the worst — an outbox
that retransmitted after an unacknowledged `DATA`, i.e. duplicate mail. Two
more were structural: **nothing in the running daemon had ever enqueued an
index job** (tasks 16–21 shipped an inert pipeline, each passing its own
gate), and `MailService.Move` silently orphans tags and notes. Re-running the
whole suite after each merge, and reading the safety-critical paths yourself,
is what catches these.

## Unfinished work preserved on branches

| Task | Branch | State |
|---|---|---|
| 23 OCR path | `worktree-agent-a92770680c94c9608` | Vision + Tesseract backends, migration and config written; untested, unreviewed. The only surviving WIP. |

Tasks 24 and 61 were previously listed here; **both are now merged and
verified** — do not re-merge them. Tasks 40, 51 and 84 were dispatched and
died to a session limit before writing anything, so there is nothing to
resume: start them fresh.

**Do not merge preserved WIP on the strength of an agent's own report.** Task
61 reported its own gate green and still contained a duplicate-mail defect
and a bypassable safety guarantee, both found at review.

## Migration numbering — assign at MERGE, not at dispatch

Numbers were originally reserved when a task was dispatched. That is broken:
tasks land out of order, and refinery only applies migrations *above* the
last-applied version, so a lower-numbered latecomer is **silently skipped** and
its table is never created. Two collisions had already been produced this way.

The rule now is: whatever number a branch used, rename it at merge to
`max_merged + 1` and fix any `-- Vnn:` references inside the file. Never
reference a migration version from Rust.

Merged: V1–V14, V16, V18–V27. V15 and V17 are permanently unused, which
costs nothing. Merged through V28. **Next free: V29.**

## Orchestration notes worth not relearning

- **Each worktree builds in its own `target/`.** Sharing one build directory
  corrupts results: cargo uplifts binaries to `target/debug/<name>`, that path
  is not keyed by source path, and `CARGO_BIN_EXE_<name>` — which the
  `rmail-cli` tests use to exec `mail` — resolves to it. See
  `.claude/BUILD_BRIEF.md`. A worktree `target/` costs 3–8 GB; delete it after
  merging the branch.
- **Reclaim Docker volumes, not just host `target/` directories.** The container
  builds into named volumes (`rmail-test-target-*`), one per worktree, which
  deleting a worktree's host-side `target/` never touches. Nine of them reached
  98.8 GB and failed a gate with "No space left on device" *inside* the
  container while the host still showed 106 GiB free. After merging a branch:
  `docker volume rm -f $(docker volume ls -q --filter name=^rmail-test-target-)`.
  Keep `rmail-test-cargo-registry`/`-cargo-git` — those are the expensive
  dependency downloads and are shared. Expect the *next* run after a prune to
  be a cold rebuild, and expect it to be the run most likely to fail: linking
  several crates at once in a 7.7 GB container OOM-killed `ld` (signal 9)
  immediately after one such prune. Re-running resumes from the compiled
  artifacts, so it is never a code change — but a plain retry only buys one
  more linked binary per run, and this workspace has 21 test binaries. Drive
  the whole link phase through serially instead, then run the suite normally:
  `scripts/docker-test.sh -- sh -c 'CARGO_BUILD_JOBS=1 cargo nextest run
  --locked --workspace --all-features --no-run'`. (`docker-test.sh` passes no
  env through, hence the `--` escape hatch.) Once every binary is linked, the
  plain `scripts/docker-test.sh` run is fine — the OOM is a *link-time* memory
  problem, not a test-time one. This is not merely a contention artifact: the
  task-76 agent hit the identical SIGKILL three times **with an exclusive
  container slot**, and only got green by narrowing to per-package runs with
  `-j 1|2`. Treat serial linking as the normal procedure, not a workaround,
  until the Docker VM gets more memory.
- **A green suite is not a race-free suite.** The full parallel run is the
  only thing on this project that has ever exercised enough scheduling
  pressure to expose a boot-ordering race — it caught one in the hook
  dispatcher (fixed in `d04ad5e`) that every targeted run had passed for
  many tasks. When a test that has never failed suddenly fails once under the
  full suite, suspect a latent race before suspecting the merge that happened
  to be in flight, and reproduce it by asserting on ordering directly rather
  than on the downstream side effect.
- **Reclaim the host side too, and do it *before* dispatching a wave.** The
  disk hit 100% (12 GiB free of 1.8 TiB) with three agents already launched
  and about to need ~7 GB of host `target/` each. Three places accumulate,
  and they are independent: Docker's named volumes (see above), each
  worktree's host-side `target/`, and the main checkout's own `target/`.
  Clearing every stale worktree's `target/` took `.claude/worktrees` from
  13 GB to 92 MB; dropping the main `target/` bought another 9 GB. All three
  are pure build output — deleting them costs a rebuild and nothing else, so
  do it freely. Removing the *checkouts* is safe too, and this note used to
  say otherwise: `git worktree remove` deletes the working directory and its
  admin files but leaves the branch alone — verified by removing one and
  confirming its ref survived, then pruning 31 more with all 35
  `worktree-agent-*` branches still present afterwards. What would lose
  preserved WIP is deleting the *branch*, which nothing here needs to do.
- **Do not union-merge structured code by deduplicating lines.** A script that
  keeps "lines from theirs not already in ours" silently drops repeated closing
  braces and attributes, which are exactly what Rust has a lot of. It happened
  to be safe for single-line `pub mod` conflicts and corrupted `main.rs` the
  first time two tasks both added subcommands. Use `git merge-file` (three-way)
  and resolve what it actually flags.
- **Quota is the binding constraint, not the machine.** Ten concurrent agents
  exhausted the session limit twice. Each agent costs roughly 0.5–0.9M tokens
  including its reviewer pass. Three or four long-lived agents get further per
  unit of quota than ten short ones.
- **Verify agent reports against the code.** One agent reported fixes it had
  not made, twice. Read the diff, not the summary.
- **Check that a new test actually bites.** Revert the fix and confirm the test
  fails. A regression guard written for the tracing bug initially passed
  against the broken build, because the value it asserted on also appeared in
  an unrelated error string.
- `rmaild/src/auth/methods.rs` fails closed, and
  `every_rpc_in_the_descriptor_set_has_a_scope_row` now reconciles it against
  the compiled protos. Any new RPC needs a row or that test fails by name.
  `AuditService` shipped denying every call before this existed.
- Bare `cargo nextest run -p rmaild <name>` filters match test *names*, not
  integration-binary ids; use `--test <name>`. Several `tasks.md` verify lines
  are written in the form that matches nothing.
