# Project constitution

Production-grade Rust gRPC service. This file is read every session — keep it lean and authoritative.

## Toolchain (source of truth for gates)
- Format: `cargo fmt --all`  /  check with `cargo fmt --all -- --check`
- Lint: `cargo clippy --all-targets --all-features -- -D warnings`
- Test: **`scripts/docker-test.sh`** — tests run in a disposable container, never on the host. The script wraps `cargo nextest run --locked --all-features --workspace`; pass extra cargo args straight through (`scripts/docker-test.sh --no-default-features`, `scripts/docker-test.sh -p rmail-core`). Never invoke `cargo test`/`cargo nextest` directly. The container is `--rm` and force-removed on exit; the cargo registry and Linux target dir persist in named volumes so runs stay incremental — `scripts/docker-test.sh --clean` purges them for a cold run.
- Build: `cargo build --release`
- Protos: `buf lint` and `buf breaking` if `buf.yaml` exists; otherwise protos build via `tonic-build` in `build.rs`
- Supply chain (final gate): `cargo deny check` and/or `cargo audit`

The `Stop` hook runs the fmt/clippy/test gate and blocks completion on failure. Never declare a task done on a red gate. fmt and clippy run on the host (fast, no side effects); tests go through the container. There is no host fallback — if the Docker daemon is down the gate blocks and says so rather than running the suite locally.

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
