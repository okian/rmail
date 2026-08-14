# Resume state — parallel build

`tasks.md` is the definitive progress tracker. This file records only what it
does not carry: which worktree branch holds unfinished work, and the
orchestration rules that were learned the hard way.

## Merged and checked off

**61 of 86 done, 25 remaining.** `tasks.md` is authoritative — count with
`grep -c '^- \[ \]' tasks.md`. Every task is verified on the *combined* tree
after merge, never on the agent's own report: currently **2529/2529** on the full
workspace suite, with clippy, `buf lint`, gitleaks and typos clean.

Defects in already-merged work keep being found by the post-merge full-suite
run or by orchestrator review rather than by the task that introduced them: a
hook-dispatcher boot race, a budget enforcer that did not bound in-flight
batches, a misspelled test name, and — the worst — an outbox that
retransmitted after an unacknowledged `DATA`, i.e. duplicate mail. Two more
were structural: **nothing in the running daemon had ever enqueued an index
job** (tasks 16–21 shipped an inert pipeline, each passing its own gate), and
`MailService.Move` destroyed every message-level tag and note (fixed in
`31ab82b` — see below). Re-running the whole suite after each merge, and
reading the safety-critical paths yourself, is what catches these.

**Where to look for the next one.** These all live in a seam no task owns.
The productive question is not "is this task correct" but "does anything
actually drive this subsystem, and what happens to the data at the boundary".
Two checks that have each paid off: grep the daemon's startup for what it
spawns and diff that against the subsystems that expose a `spawn`
(currently five, all wired and all joined at shutdown — that one is clean
now); and for any path that deletes a `messages` row, ask what cascades with
it and whether the next sync can reconstruct it. The `Move` bug was the
second check — the schema comment in V24 had *documented* the data loss and
left it, because closing it belonged to no single task.

## Unfinished work preserved on branches

**None.** 52, 77 and 84 were all resumed from their transcripts after the
session limit reset and are merged. Resuming via a message to the stopped
agent, rather than dispatching a fresh one, kept its own design context —
much cheaper than a cold start and it avoids a second agent re-deciding
questions the first had already settled.

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

Merged: V1–V14, V16, V18–V28, V32–V36. V15 and V17 are permanently unused, which
costs nothing. **Next free: V37.**

The wave of 51/64/66 all hit this and all were renumbered at merge: V30→V34,
V31→V35, and task 23's V15→V33. The V15 case is the one the collision test
does *not* catch — it was a genuine unused gap, not a duplicate — so the
renumber-at-merge rule stays written down as well as gated.

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
- **When even the serial link OOMs, narrow the build instead of retrying.**
  With three agents active, `--workspace --no-run` was itself SIGKILLed
  linking `rmaild`'s integration binaries. The escape is to stop building
  them: `./scripts/docker-test.sh -- sh -c 'CARGO_BUILD_JOBS=1 cargo nextest
  run --locked --all-features -p rmail-core --lib <filter>'` builds one test
  binary and links in seconds. For anything whose subject is in `rmail-core`
  — which is most domain work, and every revert-and-check-the-test-bites
  probe — this is strictly the better command: it turned a 3-minute link that
  failed into a sub-minute one that passed, four times in a row. Save the
  full-workspace run for the post-merge gate, where it is actually the point.
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
- **Pruning Docker volumes does not give the *host* its disk back (macOS).**
  Dropping 28 GB of named volumes took Docker's own accounting from 106.7 GB
  to 78.7 GB and moved `df` not at all: the VM's disk image is a sparse file
  that grows and never shrinks. Volume pruning is still worth doing — it is
  what stops "No space left on device" *inside* the container — but when the
  **host** is short, the levers are the checkouts' `target/` directories (the
  main one alone is ~14 GB and rebuilds in minutes) and nothing else.
- **Map a volume to its worktree before deleting it.** The name is
  `rmail-test-target-$(printf '%s' "$repo_root" | shasum | cut -c1-12)`, so
  the owner of each volume is computable rather than guessable. Deleting a
  running agent's volume forces it into a cold rebuild mid-gate; deleting the
  main checkout's throws away the 26 GB cache every orchestrator run depends
  on. Compute the hashes, then remove only the spent ones.
- **Two tasks adding one parameter each to the same function trips
  `clippy::too_many_arguments`, and the fix is not `#[allow]`.** Tasks 51 and
  64 both grew `search_service::build_hits` by one argument, taking it to 8.
  Both call sites passed identical values for everything except `presented`,
  so the honest resolution was a `HitContext` struct carrying the invariant
  half — smaller call sites, and the "only `presented` differs" fact is now
  stated in the code rather than implied.
- **Union-merging two conflicting test modules eats a closing brace.** The
  known hazard, hit again merging 51 into 64's `search_service.rs`: both sides
  had appended `#[test]` functions, and concatenating the two halves dropped
  the `}` that ended the last function on the ours side. `cargo check` caught
  it immediately (`unclosed delimiter`), but only because the result did not
  compile — a conflict in *data* rather than code would have merged silently.
- **A cross-task merge can produce a type error that neither branch had.**
  Task 51 wrote a bare `return;` in `run_stream`, correct against the
  `()`-returning signature it branched from; task 64 had since given that
  function a return value. Neither branch was wrong; the combination was.
  This is the class of defect a per-task gate structurally cannot see, and
  the reason the full suite runs on the *combined* tree after every merge.

## Open follow-ups the reviewer raised and I did not fix

From the task-52 review (all P2; the P0 and P1 were fixed in `1d950e2`):

- `AskMailbox` flattens every retrieval error to `INTERNAL`
  (`search_service.rs`'s `ask retrieval:` map), so `--account 999` is
  indistinguishable from a daemon bug. `EvalSearch` does the same, but that
  RPC is admin-only and this one takes `account_id` off the wire.
- A cancelled `AskMailbox` stream ends with a clean `OK` and no terminal
  frame: the client keeps half an answer, no citations, and exit 0.
- The `Trace` frame is emitted *behind* the concurrency permit and budget
  check, though its own docs and the proto promise it arrives up front so a
  client can render "asked over N messages" while waiting.
- No cancellation or disconnect test coverage anywhere in `rag/tests.rs` or
  `ask_mailbox.rs` — every test passes a token it never cancels. "Aborts
  upstream on cancel" is the acceptance criterion least actually proven.
- `Injected`/`serve_uds_injected` are unconditionally `pub`; a
  `#[cfg(any(test, feature = "test-util"))]` gate would make the "tests only"
  claim structural rather than documentary.
- `rmail-cli`'s `ask` prints model- and mail-authored text raw. `sanitize_model_text`
  exists and `rules::classify` already applies it; a bidi override in a body
  reorders what the user reads.

## `--no-verify` and the commit policy

Every task commit uses `--no-verify` because the user authorized it, but the
stored policy is "keep gitleaks/shellcheck/typos/fmt". The compensating
control is that all four run manually after each merge, and they earn it —
`typos` alone has caught three real entries this session (a tesseract
language code, an upstream crate's misspelled enum variant, and a zero-width
evasion fixture). If that discipline lapses the policy is simply not being
followed, so keep running them, not just the container suite.

## Verify lines: twelve done tasks were proving nothing

Task 41 swept all 96 nextest invocations in `tasks.md` against a real
`cargo nextest list` inside the container. A bare positional filter matches a
test's **name**, not its binary id — and `docker-test.sh` always injects
`--workspace`, so `-p` is ignored too. Twelve *completed* tasks had verify
lines selecting zero tests. Measured directly: `-p rmaild mail_service`
reports "no tests to run"; `--test mail_service` runs 22. `ai_service` 20,
`compose_service` 16 — 58 tests three finished tasks cited as proof and had
never once executed.

Use `--test <binary>` or `-E 'binary(<name>)'`. A module filter alone
(`search_service`) silently runs only the *lib* unit tests and none of the
integration binary, which is the quiet version of the same bug.

Also worth knowing: a verify line can go stale from an unrelated change.
Task 84's `-p rmail-cli keymap::` matched nothing after the keymap moved to
`rmail-core` to let `ConfigService` share the action registry.

## Host disk is now the binding constraint, not quota

The disk hit **100% (16 GiB of 1.8 TiB)** with two agents running. Almost none
of it is this project: the worktrees are ~3 GB and the reclaimable Docker
images a few hundred MB. Pruning volumes does not help — on this setup it
frees space *inside* the VM and returns none of it to the host.

Practical rule: with the host under ~20 GiB, **do not dispatch a third
agent**. A fresh worktree needs 2–7 GB of host `target/` plus a new Docker
volume, and running out mid-build fails every agent at once, not just the new
one. Two concurrent agents is the safe ceiling until the host has room.
