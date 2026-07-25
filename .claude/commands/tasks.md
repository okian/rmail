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
