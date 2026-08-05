# Parallel build brief

You are implementing **exactly one** numbered task from `tasks.md` as part of a parallel
build. Several sibling agents are implementing other tasks at the same time, each in its
own git worktree branched from the same commit. Everything below exists to make your work
land cleanly alongside theirs.

Read `CLAUDE.md` (project constitution) and the relevant section of `prd.md` before you
start. `tasks.md` holds your task's acceptance criteria and its `verify` command.

## Build commands

Build in **your own worktree's** `target/` — that is cargo's default, so just run `cargo`
normally. Prefix with `CARGO_INCREMENTAL=0` to keep the directory small:

```
CARGO_INCREMENTAL=0 cargo <...>
```

**Do not point `CARGO_TARGET_DIR` at another checkout's `target/`.** Sharing a build
directory across worktrees is not safe: cargo uplifts the final binaries to
`target/debug/<name>`, that path is *not* keyed by source path, and `CARGO_BIN_EXE_<name>`
— which the `rmail-cli` integration tests use to exec the `mail` binary — resolves to it.
Two worktrees building `rmail-cli` therefore overwrite each other's binary, and a test can
silently run a sibling's build. Your own `target/` costs a few GB and there is room.

A full first build takes a while. Pass `timeout: 900000` to the Bash tool on cargo commands.

## The gate — you must run it yourself and it must be green

```
CARGO_INCREMENTAL=0 cargo fmt --all -- --check
CARGO_INCREMENTAL=0 cargo clippy --all-targets --all-features -- -D warnings
CARGO_INCREMENTAL=0 cargo nextest run --all-features --workspace
buf lint
```

Plus your task's own `verify` line. Do not finish on a red gate. If a *pre-existing*
failure is unrelated to your task, say so explicitly in your report rather than papering
over it.

## Non-negotiables (from CLAUDE.md — these are hard review gates)

- No `unwrap()`, `expect()`, `panic!`, `todo!()`, `unimplemented!()` in non-test code.
  The workspace lints deny them; do not add `#[allow]` to get around it.
- Domain errors via `thiserror` in `rmail-core`; map to `tonic::Status` **only** at the
  gRPC boundary, with the right code and a stable `ErrorInfo.reason` (see
  `rmail-core/src/error.rs` — reuse the existing `Error`/`ErrorReason`, do not invent a
  parallel error type).
- Async on tokio; never block the runtime. CPU-heavy or `rusqlite` work goes on
  `spawn_blocking`. Honor cancellation — thread the request's `CancellationToken`
  through; never leak a task.
- `tracing` spans with structured fields on every request path. No `println!`/`eprintln!`.
- Config through the typed structs in `rmail-core/src/config/`, never hardcoded. New
  knobs get defaults matching `prd.md` and an entry in `.env.example` where env-settable.
- Every acceptance bullet needs a test that actually runs and would fail if the behavior
  regressed. Cover error/`Status` paths, not just the happy path. gRPC tests run against
  an in-process tonic server (see `rmaild/tests/` for the existing harness), never a mock
  of the transport.
- No stubs, no `TODO`, no dead placeholder code. The task ships complete.

## House style

Match the surrounding code. This codebase is heavily commented in a specific register:
comments explain *why* a design choice was made and what breaks otherwise, never what the
line does. Module-level `//!` docs open with the design rationale. Public items carry
rustdoc. Tests live in a sibling `tests.rs` (`mod tests;` behind `#[cfg(test)]`) matching
the existing layout — see `rmail-core/src/index/` for the pattern. Test helpers are
hand-rolled (there is no `tempfile` dependency; see `rmail-core/src/storage/tests.rs`).

## Staying out of your siblings' way

- **Protos**: `rmail-proto/build.rs` globs `proto/rmail/v1/*.proto`. Put your service in a
  **new** file named after it. Do not edit `build.rs`. Only edit an existing proto if your
  task explicitly extends that service.
- **Migrations**: your task prompt assigns you a reserved `V<N>__` number. Use exactly
  that number, and only that one. Never renumber an existing migration.
- **Shared files** (`rmail-core/src/lib.rs`, `rmaild/src/lib.rs`, `rmail-cli/src/main.rs`,
  `Cargo.toml`): touch them as little as you can, and keep each change to a minimal,
  self-contained line or block. Add new dependencies to `[workspace.dependencies]` and
  reference them as `foo.workspace = true` in the member crate. Sibling agents are editing
  these same files, so a small diff is the difference between a clean merge and a manual
  one.
- Create only the modules your task owns.

## Finishing

1. Gate green (above).
2. Invoke the `reviewer` subagent on your diff and fix every finding worth fixing.
   Re-run the gate afterwards.
3. Commit **on your worktree branch** with a Conventional Commit scoped to the task, e.g.
   `feat(search): lexical BM25 retriever`. One commit for the task is ideal; a small number
   is fine. Use `git commit --no-verify`.
4. Do **not** check the box in `tasks.md` — the orchestrator does that after merging.
5. Report back: what you built, the gate result, anything the orchestrator must know to
   merge (files you touched that siblings likely also touched, new deps, new config keys,
   new proto files, follow-on work you deliberately left to a later task).
