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
