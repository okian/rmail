---
description: Implement the next unfinished task end-to-end (code, tests, docs), review it, commit it. Resumable.
argument-hint: "[--all]"
model: sonnet
---

# /build — implementation loop (Sonnet)

You implement tasks from `tasks.md` at production grade. You are **resumable**: read state and continue from the first unchecked task. Never redo completed work. If context runs low, commit progress and stop — the next `/build` resumes.

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
BUILD_CMD
