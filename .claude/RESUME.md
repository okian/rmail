# Resume state — parallel build

`tasks.md` is the definitive progress tracker. This file records only what it
does not carry: which worktree branch holds unfinished work, and the
orchestration rules that were learned the hard way.

## Merged and checked off

Tasks **24 is not done**; merged so far are **25, 26, 27, 28, 38, 39, 43, 44,
45, 46, 47, 86**. Each was verified on the *combined* tree after merge, not
merely in its own worktree: 1080/1080 tests, clippy clean, `cargo deny` and
`cargo audit` clean, `buf lint` + `buf breaking` clean, gitleaks clean.

## Unfinished work preserved on branches

| Task | Branch | State |
|---|---|---|
| 23 OCR path | `worktree-agent-a92770680c94c9608` | Vision + Tesseract backends, migration and config written; untested, unreviewed |

Tasks 24 (IndexService), 29 (fusion), 48 (triage) were dispatched and died to a
usage limit before writing anything — start them fresh or resume the agent.

## Migration numbering — assign at MERGE, not at dispatch

Numbers were originally reserved when a task was dispatched. That is broken:
tasks land out of order, and refinery only applies migrations *above* the
last-applied version, so a lower-numbered latecomer is **silently skipped** and
its table is never created. Two collisions had already been produced this way.

The rule now is: whatever number a branch used, rename it at merge to
`max_merged + 1` and fix any `-- Vnn:` references inside the file. Never
reference a migration version from Rust.

Merged: V1–V14, V16, V18 (ai_ledger), V19 (retrievers), V20 (ai_queue).
V15 and V17 are permanently unused, which costs nothing. **Next free: V21.**

## Orchestration notes worth not relearning

- **Each worktree builds in its own `target/`.** Sharing one build directory
  corrupts results: cargo uplifts binaries to `target/debug/<name>`, that path
  is not keyed by source path, and `CARGO_BIN_EXE_<name>` — which the
  `rmail-cli` tests use to exec `mail` — resolves to it. See
  `.claude/BUILD_BRIEF.md`. A worktree `target/` costs 3–8 GB; delete it after
  merging the branch.
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
