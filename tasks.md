# rmail — Work Breakdown

Decomposed from `prd.md` (v0.2). Ordered by dependency; the first task scaffolds the
workspace/toolchain so everything after it is verifiable. `/loop` implements the first
unchecked task, reviews it, commits it.

Status legend: `- [ ]` todo · `- [x]` done. Do not reorder IDs — `depends-on` references them.

Crates (established in task 1): `rmail-proto` (generated protos), `rmail-core`
(domain + storage + sync + index + search + ai), `rmaild` (daemon / gRPC server),
`rmail-cli` (the `mail` binary — thin gRPC client).

Global gate (Stop hook enforces on every task): `cargo fmt --all -- --check` ·
`cargo clippy --all-targets --all-features -- -D warnings` ·
`cargo nextest run --workspace` (fallback `cargo test --all-features --workspace`).
Per-task **verify** lists the *targeted* proof in addition to the global gate.

---

## 1. Workspace & toolchain scaffold
- [x] status
- **depends-on:** none
- **parallel-safe:** no
- **acceptance:**
  - Cargo workspace with member crates `rmail-proto`, `rmail-core`, `rmaild`, `rmail-cli` (bin `mail`); workspace-level `[workspace.lints]` denying `clippy::unwrap_used`/`expect_used`/`panic`/`todo` in non-test code.
  - `rustfmt.toml` and clippy lint config committed; repo formats and lints clean.
  - `proto/rmail/v1/` holds a versioned package `rmail.v1` with a trivial `HealthPing` message; `rmail-proto/build.rs` compiles it via `tonic-build`; `buf.yaml` present and `buf lint` passes.
  - `rmaild` boots a minimal tonic server on a Unix domain socket (`0600`) exposing `tonic-health` and `tonic-reflection`; graceful shutdown on SIGINT/SIGTERM.
  - `mail` binary exists with a `ping` subcommand that round-trips gRPC `Health.Check` over the socket.
  - `nextest` configured; a placeholder integration test starts the in-process server and asserts health `SERVING`.
  - GitHub Actions CI workflow runs fmt-check, clippy, nextest, `buf lint`, and `cargo build --release`; `.env.example` committed.
- **verify:** `cargo build --release` · `buf lint` · `cargo nextest run -p rmaild health` · `cargo fmt --all -- --check` · `cargo clippy --all-targets --all-features -- -D warnings`

## 2. Configuration system
- [x] status
- **depends-on:** 1
- **parallel-safe:** yes
- **acceptance:**
  - Typed config structs (serde) for the master TOML (accounts, sync, search, index, ai, tags, send, finder, grpc) loaded via `figment`/`config`: file + env overlay, no hardcoded secrets.
  - Secret material sourced only via `password_command`/env/keychain reference — never inline; `.env.example` documents every env knob.
  - Defaults match the PRD example config; unknown keys are rejected with a clear error (thiserror), not silently ignored.
  - Config is hot-reload-friendly (parse is a pure function returning `Config`, no globals).
- **verify:** `cargo nextest run -p rmail-core config::` (covers defaults, env override, missing-file, bad-value error path)

## 3. Error model & tonic Status mapping
- [x] status
- **depends-on:** 1
- **parallel-safe:** yes
- **acceptance:**
  - Core `thiserror` error enum for domain/library errors; no `anyhow` below the binary top level.
  - Boundary mapper `Error -> tonic::Status` attaching `google.rpc.ErrorInfo` with a stable `reason` enum (`UNAUTHENTICATED`, `PERMISSION_DENIED`, `NOT_FOUND`, `FAILED_PRECONDITION`, `UNAVAILABLE`, `RESOURCE_EXHAUSTED`, `DEADLINE_EXCEEDED`, `ALREADY_EXISTS`).
  - Every variant maps to the correct gRPC code; clients can branch on `reason`, never message text.
- **verify:** `cargo nextest run -p rmail-core error::` (asserts each variant → expected code + reason)

## 4. Tracing & observability baseline
- [x] status
- **depends-on:** 1
- **parallel-safe:** yes
- **acceptance:**
  - `tracing` + `tracing-subscriber` initialized in `rmaild`; env-filtered levels; structured JSON option.
  - Request/span helpers with structured fields (account, mailbox, request-id); zero `println!`/`eprintln!` for logging anywhere.
  - A test asserts logs are emitted through the subscriber (capture layer), not stdout prints.
- **verify:** `cargo nextest run -p rmail-core telemetry::` · `! grep -rn 'println!\|eprintln!' rmail-core/src rmaild/src` (no matches outside tests)

## 5. SQLite storage foundation & migrations
- [x] status
- **depends-on:** 1, 2
- **parallel-safe:** no
- **acceptance:**
  - `rusqlite` in WAL mode; a single-writer connection + a pooled set of read connections (search never blocks on writes).
  - `refinery` migration runner wired; migrations directory established; `mail`/`rmaild` run pending migrations on open, idempotently.
  - Busy-timeout, foreign-keys ON, and sane pragmas set on every connection.
  - Fault-injection test: a failed migration rolls back cleanly and reports via the error model.
- **verify:** `cargo nextest run -p rmail-core storage::` (WAL enabled, read pool concurrency, migration up/rollback)

## 6. Baseline core schema
- [x] status
- **depends-on:** 5
- **parallel-safe:** no
- **acceptance:**
  - Migrations create `accounts`, `mailboxes`, `messages`, `flags`, `attachments`, `threads`, `contacts`, `sync_state` per the PRD data model (stable `messages.id`, raw RFC822 blob column, parsed metadata, UID/UIDVALIDITY fields).
  - Indexes for the hot paths (`messages(date)`, UID lookups, thread refs) created.
  - Typed row structs + basic repository accessors (insert/get/list) with tests.
- **verify:** `cargo nextest run -p rmail-core schema:: repo::`

## 7. Account model & credential providers
- [x] status
- **depends-on:** 3, 6
- **parallel-safe:** no
- **acceptance:**
  - `rmaild` (and `mail` where it opens local state) open the `Database` (task 5) at startup at a resolved data-dir path, running migrations idempotently — the first DB consumer wires this.
  - Account CRUD over `accounts`; credential resolution via macOS Keychain (`security-framework`), `password_command`, and env — resolved lazily, never persisted in plaintext.
  - `AccountService` gRPC skeleton (`Create/List/Get/Delete/TestConnection` returning `UNIMPLEMENTED` where not yet wired) attached to the daemon.
  - Server installs a `tower`/`tonic` layer that opens a `telemetry::request_span` (request-id/account/mailbox fields) per RPC — the first real RPCs land here, so the deferred request-trace layer from task 4 is wired now.
  - Redaction: credentials never appear in logs or `Debug`.
- **verify:** `cargo nextest run -p rmail-core account:: credential::` · `cargo nextest run -p rmaild account_service`

## 8. IMAP connection, login & folder listing
- [x] status
- **depends-on:** 3, 4, 7
- **parallel-safe:** no
- **acceptance:**
  - `async-imap` over `rustls` TLS; connect + LOGIN/AUTHENTICATE; capability probe (IDLE, CONDSTORE, QRESYNC, MOVE).
  - `AccountService.TestConnection` verifies login; folder discovery populates `mailboxes`.
  - Auth failure / unreachable host map to `UNAVAILABLE`/`UNAUTHENTICATED`; the daemon stays up and local features remain usable.
  - Tests run against a mock/in-process IMAP server (no live network).
- **verify:** `cargo nextest run -p rmail-core imap::conn imap::folders`

## 9. Message fetch, parse & persist
- [x] status
- **depends-on:** 6, 8
- **parallel-safe:** no
- **acceptance:**
  - FETCH RFC822 + flags + internaldate; parse via `mail-parser` (multipart, quoted-printable, base64, encoded headers, UTF-8); store raw + parsed metadata + attachment metadata + body text.
  - Idempotent upsert keyed by (account, mailbox, UID, UIDVALIDITY); re-fetch is a no-op.
  - HTML bodies stored with a stripped-text projection (`html2text`) for later indexing.
- **verify:** `cargo nextest run -p rmail-core message::fetch message::parse` (fixtures for multipart/encoded/HTML mail)

## 10. Threading model
- [x] status
- **depends-on:** 9
- **parallel-safe:** no
- **acceptance:**
  - `threads` populated from `Message-ID`/`In-Reply-To`/`References`; subject-normalization fallback for missing references.
  - Stable thread ids; membership updates as new messages arrive; participant set derived.
- **verify:** `cargo nextest run -p rmail-core thread::` (reference chains, subject fallback, out-of-order arrival)

## 11. Full folder sync (initial)
- [x] status
- **depends-on:** 9, 10
- **parallel-safe:** no
- **acceptance:**
  - Initial sync walks a folder by UID window, downloading and persisting all messages with bounded concurrency and resumable checkpoints in `sync_state`.
  - Recent/INBOX prioritized first so results are useful early; progress is observable.
  - Re-running is incremental (already-synced UIDs skipped).
- **verify:** `cargo nextest run -p rmail-core sync::full` (fresh sync, resume mid-window, re-run no-op)

## 12. CONDSTORE/QRESYNC delta sync
- [x] status
- **depends-on:** 11
- **parallel-safe:** yes
- **acceptance:**
  - Persist per-folder `UIDVALIDITY` + `HIGHESTMODSEQ`; issue QRESYNC / `FETCH … CHANGEDSINCE` so only changes transfer; detect expunges and flag changes.
  - Fallback to a UID-window diff on servers lacking CONDSTORE/QRESYNC; UIDVALIDITY change triggers a safe resync.
- **verify:** `cargo nextest run -p rmail-core sync::qresync sync::uiddiff_fallback`

## 13. IMAP IDLE push engine
- [x] status
- **depends-on:** 11
- **parallel-safe:** yes
- **acceptance:**
  - Long-lived IDLE connection per high-priority folder; new mail / flag change / expunge reflected within seconds; connection kept alive with periodic re-IDLE.
  - Transparent degradation to interval polling when IDLE is unavailable; reconnect with backoff on drop.
- **verify:** `cargo nextest run -p rmail-core sync::idle sync::poll_fallback`

## 14. Durable event log & in-process bus
- [x] status
- **depends-on:** 6
- **parallel-safe:** yes
- **acceptance:**
  - `events` table (monotonic `seq`, kind, payload) as the durable source powering resumable subscriptions; retention by rows/days.
  - In-process `tokio::sync::broadcast` fan-out; a subscriber can resume from `since_seq` with no gaps, and receives `OUT_OF_RANGE`+`oldest_seq` when the cursor is past retention.
- **verify:** `cargo nextest run -p rmail-core events::` (append, resume-from-cursor, retention gap signal)

## 15. SyncService gRPC (sync/status/pause + WatchEvents)
- [x] status
- **depends-on:** 12, 13, 14
- **parallel-safe:** no
- **acceptance:**
  - `SyncService.SyncFolder`, `Status`, `Pause/Resume`, and `WatchEvents` (server-stream over the durable log) wired to the daemon; `mail sync [--full] [--watch]` CLI verb.
  - Owns wiring task 14's `EventLog` into the daemon: construction from `[grpc.events]` config, the sync engines appending to it, and a scheduled `prune()`. Task 14 ships the engine; nothing calls it until here.
  - **Deferred to task 13's follow-up / the background scheduler:** the IDLE watcher is not spawned by the daemon, so events are produced only by an explicit `SyncFolder` RPC. `SyncEngine::sync` has no injection seam for a mock IMAP server, so its success path (connect → pass → drain → report) is covered by the engines beneath it rather than end to end. `SyncFolder` carries no server-side deadline.
  - Sync emits `events` (NewMessage/FlagsChanged/SyncProgress) that downstream indexing/AI/rules consume.
  - Client cancellation stops the upstream stream promptly.
- **verify:** `cargo nextest run -p rmaild sync_service` (in-process server: trigger sync, observe streamed events, resume)

## 16. Index job queue & state
- [x] status
- **depends-on:** 6, 14
- **parallel-safe:** no
- **acceptance:**
  - `index_queue` + `index_state` per the PRD; work keyed by `(message_uid, index_kind, content_hash)`; re-run is a no-op unless content changed or embed model changed.
  - Durable, resumable, lease-with-expiry reaping on restart; poison jobs quarantined after max attempts without head-of-line blocking.
  - Priority for recent/INBOX mail; sync enqueues, workers drain, UI never blocks.
- **verify:** `cargo nextest run -p rmail-core index::queue` (dedup, lease reclaim, content-hash short-circuit, poison quarantine)

## 17. Text extraction stage
- [x] status
- **depends-on:** 16
- **parallel-safe:** no
- **acceptance:**
  - `index_content` populated per part (subject/headers/body/note/summary) with normalized text, `content_hash`, extractor, lang; HTML stripped.
  - Idempotent by content hash; emits follow-on lexical/entity/semantic jobs.
- **verify:** `cargo nextest run -p rmail-core index::extract`

## 18. Lexical FTS5 index
- [x] status
- **depends-on:** 17
- **parallel-safe:** yes
- **acceptance:**
  - Contentless `fts_messages` FTS5 (subject, sender, recipients, body, attachments, notes, summary) with `unicode61 remove_diacritics 2`; field-weighted BM25 with TOML-configurable weights (subject 8.0 … body 1.0).
  - Insert/update/delete keep FTS in sync with `index_content`; a subject hit outranks a body hit.
- **verify:** `cargo nextest run -p rmail-core index::fts` (BM25 field weighting, phrase query, delete/update sync)

## 19. Entity extraction (regex)
- [x] status
- **depends-on:** 17
- **parallel-safe:** yes
- **acceptance:**
  - `entities`/`entity_mentions`/`entity_edges` populated by deterministic extractors: emails, phones, URLs, amounts, dates, tracking numbers, order/invoice IDs, IBANs; normalized `norm` + `UNIQUE(kind,norm)`.
  - Spans recorded for highlighting; confidence set; graceful skip on binary/empty parts.
- **verify:** `cargo nextest run -p rmail-core index::entities` (fixtures per entity kind, normalization, dedup)

## 20. Embedder trait & local embeddings
- [x] status
- **depends-on:** 2
- **parallel-safe:** yes
- **acceptance:**
  - `Embedder` trait (`model()/dim()/embed()`); default local backend (`fastembed`/`ort` ONNX, `bge-small-en-v1.5` 384d) fully offline; Voyage backend behind config with `api_key_command`.
  - Batch embedding; warm-up at daemon start; deterministic dims per model.
- **verify:** `cargo nextest run -p rmail-core embed::local` (embed batch, stable dim, offline)

## 21. Chunking & semantic vector index
- [x] status
- **depends-on:** 17, 20
- **parallel-safe:** no
- **acceptance:**
  - `chunks` (512 tok / 64 overlap) + `vec_chunks` (`sqlite-vec` `vec0`) storing per-chunk model/dim; cosine kNN over message + chunk embeddings.
  - Model/dim change triggers targeted re-embed; `index verify` flags drift; embeddings re-computed only on `content_hash` change.
- **verify:** `cargo nextest run -p rmail-core index::semantic` (chunk boundaries, kNN recall on fixtures, re-embed on model switch)

## 22. Attachment text extraction pipeline
- [x] status
- **depends-on:** 17
- **parallel-safe:** yes
- **acceptance:**
  - Format-specific extractors (PDF via `lopdf`/pdfium, DOCX, XLSX via `calamine`, PPTX, TXT, HTML, CSV) emit plain text + per-page offsets into `index_content`, content-hash keyed, mirrored to FTS.
  - Oversized/binary/encrypted attachments recorded with empty text and never block the pipeline; `max_attachment_mb` respected.
- **verify:** `cargo nextest run -p rmail-core attach::extract` (per-format fixtures, oversized skip, page offsets)

## 23. OCR path for images & scanned PDFs
- [ ] status
- **depends-on:** 22
- **parallel-safe:** yes
- **acceptance:**
  - Image attachments and text-less PDFs routed to OCR (Apple Vision default, Tesseract fallback) producing searchable text + bounding boxes; native-vs-OCR provenance and confidence recorded; opt-in via config.
- **verify:** `cargo nextest run -p rmail-core attach::ocr` (fixture image → text, provenance flag)

## 24. IndexService gRPC + `mail index` CLI
- [ ] status
- **depends-on:** 16, 18, 19, 21
- **parallel-safe:** no
- **acceptance:**
  - `IndexService.Status/Reindex(stream)` plus `mail index status|run|start|stop|reindex|rebuild|verify|gc|embed --backfill` and `mail entities <kind>`.
  - `status` reports per-kind coverage %, queue depth, model/dim, and lag; `verify` detects state/content-hash drift; `gc` vacuums orphans.
- **verify:** `cargo nextest run -p rmaild index_service` (status coverage, reindex stream, verify drift)

## 25. Query understanding — operator parser & grammar
- [x] status
- **depends-on:** 2, 3
- **parallel-safe:** no
- **acceptance:**
  - Parser for the operator grammar (`from:`,`to:`,`cc:`,`subject:`,`body:`,`has:`,`filename:`,`larger:`/`smaller:`,`before:`/`after:`/`on:`/`date:`,`is:`,`tag:`,`note:`,`in:`,`account:`,`thread:`,`ai:`, quotes, `-` negation, `~`/`=` mode sigils).
  - Operators become hard filters (WHERE); free text becomes ranked terms/phrases; unknown `key:value` degrades to free text (never an error).
- **verify:** `cargo nextest run -p rmail-core query::parse` (each operator, negation, phrase, unknown-key passthrough)

## 26. Query understanding — QueryPlan assembly
- [ ] status
- **depends-on:** 25, 19
- **parallel-safe:** no
- **acceptance:**
  - Deterministic pipeline producing `QueryPlan{hard_filters, lexical_terms, phrases, expansions, query_vector?, entities, intent, sort, scope}`.
  - Intent classification (navigational/exploratory/lookup) via a cheap local feature logistic; SymSpell/trigram spell-fix against corpus vocabulary; alias/contact resolution to soft boosts; PMI synonym expansion.
  - Query embedded once (local) for the dense retriever; Claude NL-compile is a stubbed fallback flag (wired in task 43/58).
- **verify:** `cargo nextest run -p rmail-core query::plan` (intent labels, spellfix, expansion, plan shape)

## 27. Lexical BM25 retriever
- [ ] status
- **depends-on:** 18, 25
- **parallel-safe:** no
- **acceptance:**
  - Retriever over `fts_messages` returning top-N with source-local BM25 score+rank; honors hard filters as a candidate mask; phrase/`NEAR` proximity and an unquoted proximity bonus.
- **verify:** `cargo nextest run -p rmail-core retrieve::lexical`

## 28. Candidate generation — remaining retrievers
- [ ] status
- **depends-on:** 19, 21, 26, 27
- **parallel-safe:** no
- **acceptance:**
  - Dense kNN (chunk→message, keeping max/mean similarity), fuzzy (nucleo + trigram), entity match, structured filter (hard gate), prefix/autocomplete, and recency-prior retrievers each return top-N with source score+rank.
  - All run concurrently on a bounded pool; each is individually skippable (config/degradation); a query-generation token cancels superseded scans.
- **verify:** `cargo nextest run -p rmail-core retrieve::` (each retriever + parallel fan-out + cancellation)

## 29. Fusion & dedup (RRF + SimHash)
- [ ] status
- **depends-on:** 28
- **parallel-safe:** no
- **acceptance:**
  - Weighted RRF (`k=60`, intent-dependent per-source weights) over all sources; chunk→message and optional message→thread collapse; SimHash near-duplicate collapse.
  - Linear-blend fusion available via `fusion="linear"`; output carries every source's rank+score for downstream features.
- **verify:** `cargo nextest run -p rmail-core fuse::` (RRF math, intent weights, dedup, near-dup collapse)

## 30. Feature extraction
- [ ] status
- **depends-on:** 29
- **parallel-safe:** no
- **acceptance:**
  - Per-candidate feature vector (textual/semantic/personal/temporal/status/structural/global groups) computed cheaply from local DB + fused metadata; deterministic and serializable for replay.
- **verify:** `cargo nextest run -p rmail-core features::` (vector completeness, serialization round-trip)

## 31. L1 deterministic ranker
- [ ] status
- **depends-on:** 30
- **parallel-safe:** no
- **acceptance:**
  - Cold-start linear scorer with the PRD weights (TOML-overridable) scoring all fused candidates and keeping top-K (50); pure-Rust microsecond inference; newsletter/automated down-weight gated by intent.
  - Pluggable behind a `Ranker` trait so a learned model (task 65) can hot-swap.
- **verify:** `cargo nextest run -p rmail-core rank::l1` (weighted score, top-K cut, intent gating)

## 32. Diversify & present
- [ ] status
- **depends-on:** 31
- **parallel-safe:** no
- **acceptance:**
  - MMR (λ=0.7) for exploratory intent, disabled for navigational; thread grouping with `+N` affordance; near-dup collapse chip; snippet extraction + query-term highlight (FTS5 `snippet()` / best chunk).
  - Results emitted best-first in score-ordered batches (streaming-ready).
- **verify:** `cargo nextest run -p rmail-core present::` (MMR diversity, snippet/highlight, streaming order)

## 33. SearchService gRPC (streaming) + Explain
- [ ] status
- **depends-on:** 32
- **parallel-safe:** no
- **acceptance:**
  - `SearchService.Search(stream SearchHit)`, `Semantic`, and `Explain` wired end-to-end through the pipeline; first hit reaches the client fast; a fresh request cancels the prior stream (generation token).
  - `SearchHit` carries score, highlighted snippet, `sources`, and (when `explain`) a `RankExplanation` of top feature contributions + matched spans.
- **verify:** `cargo nextest run -p rmaild search_service` (streamed hits, cancellation, explain block)

## 34. Search CLI verbs
- [ ] status
- **depends-on:** 33
- **parallel-safe:** yes
- **acceptance:**
  - `mail search "<q>"`, `--explore`, `--explain`, `--json`, `~`/`=` prefixes, and `mail similar <id>` implemented as gRPC-client verbs; `--json` emits the PRD item schema (uid, subject, score, snippet, sources, why).
- **verify:** `cargo nextest run -p rmail-cli search_cli` (json schema, flags map to request fields)

## 35. Saved searches & deterministic smart folders
- [ ] status
- **depends-on:** 33, 6
- **parallel-safe:** yes
- **acceptance:**
  - Named saved searches persisted and re-runnable through the full pipeline; deterministic smart folders (operator-DSL predicate) re-evaluated on each sync so membership stays live without moving server mail; can trigger auto-tag/notify on new matches. (NL-compiled smart folders land in task 58.)
- **verify:** `cargo nextest run -p rmail-core saved_search:: smart_folder::`

## 36. Query/embedding/result caching & incrementality
- [ ] status
- **depends-on:** 33
- **parallel-safe:** yes
- **acceptance:**
  - Query-plan cache (normalized hash), embedding cache (persist doc/query vectors, re-embed on content_hash change), and result cache keyed by `(query, filter, corpus_version)` invalidated on corpus bump/ranker change; freshly-synced mail bypasses the result cache.
- **verify:** `cargo nextest run -p rmail-core cache::` (hit/miss, corpus-version invalidation, fresh-mail bypass)

## 37. Evaluation harness + CI regression guard
- [ ] status
- **depends-on:** 33
- **parallel-safe:** yes
- **acceptance:**
  - Versioned golden set `(query, judged-relevant ids)`; `mail search eval` reports NDCG@10, MRR, Recall@50, P@3; offline replay/shadow scoring over logged impressions.
  - CI job runs the golden set on a fixture corpus and fails the build on an NDCG@10 drop below threshold.
- **verify:** `cargo nextest run -p rmail-core eval::` · `mail search eval` on the fixture corpus meets threshold

## 38. Capability tokens & auth interceptor
- [ ] status
- **depends-on:** 6, 3
- **parallel-safe:** no
- **acceptance:**
  - `api_tokens` (argon2id hashes, scopes, expiry, revoked); `AdminService.MintToken/RevokeToken/ListTokens`; `mail token create/list/revoke`.
  - tonic interceptor enforces per-method scope (`mail.read`,`mail.write`,`mail.send`,`ai.invoke`,`ai.spend:<usd>`,`mailbox:<name>`,`automation`,`admin`); Unix-socket peer-uid (`SO_PEERCRED`) grants implicit admin; TCP requires Bearer token (constant-time verify) or mTLS.
  - A read-only token is physically denied `Send`/`Delete`.
- **verify:** `cargo nextest run -p rmaild auth::` (scope allow/deny matrix, peer-uid path, revoked token rejected)

## 39. MailService
- [ ] status
- **depends-on:** 9, 10, 14, 38
- **parallel-safe:** no
- **acceptance:**
  - `List(stream)`, `Get`, `GetThread`, `Move`, `Copy`, `SetFlags`, `Delete`, `GetAttachment(stream)`, `WatchEvents(stream)` implemented over core services with correct scopes; attachments chunk-streamed within the 16 MiB frame cap.
  - Mutations reflect to IMAP (flags/move) and emit `events`.
- **verify:** `cargo nextest run -p rmaild mail_service` (CRUD, threaded get, watch stream, attachment chunking)

## 40. Idempotency, pagination & error-model hardening
- [ ] status
- **depends-on:** 39
- **parallel-safe:** no
- **acceptance:**
  - `idempotency_keys` table; mutating RPCs accept an `idempotency_key` — same key+hash replays the cached response, differing payload → `ALREADY_EXISTS`.
  - Server-capped `page_size` (≤500) + opaque `page_token` on list RPCs; all error paths carry stable `ErrorInfo.reason`.
- **verify:** `cargo nextest run -p rmaild idempotency:: pagination::`

## 41. Feature-parity command enum + CI drift check
- [ ] status
- **depends-on:** 39, 33
- **parallel-safe:** no
- **acceptance:**
  - A single internal command enum backs CLI/TUI/gRPC; a test enumerates every core command and asserts a corresponding RPC exists.
  - CI job fails if any core command lacks an RPC (no CLI/gRPC/MCP feature drift).
- **verify:** `cargo nextest run -p rmail-core parity::` (every command → RPC; missing mapping fails)

## 42. CLI as gRPC client: structured output & generic call
- [ ] status
- **depends-on:** 38, 40
- **parallel-safe:** no
- **acceptance:**
  - Global `--format {table,json,ndjson}` on every command with stable serde schemas and stable exit codes; streaming commands emit ndjson mirroring gRPC frames.
  - `rmail daemon start|status|stop`, `rmail api ping|reflect|call <Method> <json>`, and global flags (`--socket`,`--addr`,`--token`,`--tls-*`,`--insecure`,`--deadline`); daemon auto-start or `FAILED_PRECONDITION`.
- **verify:** `cargo nextest run -p rmail-cli format:: api_call::` (format stability, generic call via reflection, exit codes)

## 43. Anthropic provider & Provider trait
- [x] status
- **depends-on:** 2, 3, 4
- **parallel-safe:** no
- **acceptance:**
  - `Provider` trait; Claude backend over the Anthropic Messages API (`reqwest`+`rustls`) using models `claude-haiku-4-5`/`claude-sonnet-5`/`claude-opus-4-8`; API key via `api_key_command`.
  - Structured output via `output_config.format` (`json_schema`, `strict`) — no regex on model output; SSE deltas mapped 1:1 to typed stream frames (Token/ToolUseStart/Usage/Done) with upstream abort on cancel/deadline.
  - Prompt caching (frozen system+schema prefix, `cache_control: ephemeral` ttl 1h) with `usage.cache_read_input_tokens` verification; retry with exponential backoff + jitter; `refusal` → error, no retry.
- **verify:** `cargo nextest run -p rmail-core ai::provider` (against a mock Anthropic server: structured decode, streaming frames, cache headers, retry/backoff, cancel)

## 44. PII redaction firewall
- [ ] status
- **depends-on:** 43
- **parallel-safe:** no
- **acceptance:**
  - Mandatory pre-flight over every body/thread before any Claude call: reversibly tokenizes emails, phones, cards (Luhn), addresses, secrets, names in memory; re-hydrates the model response so the user sees real values but the API never receives raw PII.
  - Empty-after-redaction short-circuits to `redacted_skip`; `redact_preview` surface exposes what would be sent.
- **verify:** `cargo nextest run -p rmail-core ai::redact` (tokenize/rehydrate round-trip, Luhn, no raw PII in outbound payload)

## 45. AI audit ledger + usage/cost accounting
- [ ] status
- **depends-on:** 6, 43
- **parallel-safe:** yes
- **acceptance:**
  - Append-only ledger recording every Claude request (timestamp, ids, model, tokens, cost, redaction level, latency, SHA-256 of the exact payload sent); `ai_usage` day rollups; every AI artifact links to its ledger entry.
  - `AuditService.QueryAiCalls/ExportLedger`.
- **verify:** `cargo nextest run -p rmail-core ai::audit` (append-only invariant, payload hash, cost rollup)

## 46. AI policy & data-residency engine
- [ ] status
- **depends-on:** 2, 6
- **parallel-safe:** yes
- **acceptance:**
  - Declarative per-account/folder/pattern `allowed | local-only | forbidden` + residency tag; every AI path consults it first; forbidden folders are invisible to AI features; every resolution is logged and `explain`-able.
- **verify:** `cargo nextest run -p rmail-core ai::policy` (allow/deny/local-only resolution, forbidden invisibility, explain trace)

## 47. AI queue & worker pool
- [ ] status
- **depends-on:** 16, 43, 44, 45, 46
- **parallel-safe:** no
- **acceptance:**
  - Persistent `ai_queue` (dedup `UNIQUE(message_id,pass)`), lease model with expiry reaping, `Semaphore(max_concurrency)` + token-bucket RPM limiter; cost gate against `ai_usage[today]` applying `on_cap` (pause/triage_only/drop).
  - Batch mode flips to the Message Batches API when depth ≥ threshold (`custom_id = message_id`, 50% cost); offline rows stay `pending` and drain on reconnect; provider 429/5xx → backoff then `dead`, `mail ai retry --failed` requeues.
- **verify:** `cargo nextest run -p rmail-core ai::queue` (dedup, lease reclaim, RPM/cost gate, batch flip, retry→dead)

## 48. Triage pass (Haiku)
- [ ] status
- **depends-on:** 47
- **parallel-safe:** no
- **acceptance:**
  - Every newly synced message runs one Haiku structured-JSON call → category, priority, `needs_reply`, sentiment, suggested tags, `tl_dr` written to `ai_summaries`; `ai_fts` FTS index over AI fields; `ai:` search operators functional.
- **verify:** `cargo nextest run -p rmail-core ai::triage` (schema-valid output, ai_fts populated, `ai:needs-reply` filter)

## 49. Deep pass + thread-aware summary
- [ ] status
- **depends-on:** 48
- **parallel-safe:** no
- **acceptance:**
  - Conditional deep pass (Opus/Sonnet) when triage flags priority≥high / needs_reply / allowlisted category: summary, key_points, todos, entities/dates/amounts, suggested_reply, incremental thread summary folding prior `ai_summaries.summary` for the thread.
  - Enrichments feed the lexical + semantic indexes.
- **verify:** `cargo nextest run -p rmail-core ai::deep` (gating logic, thread rollup incrementality, index feed)

## 50. AiService gRPC + streaming RPCs
- [ ] status
- **depends-on:** 48, 49
- **parallel-safe:** no
- **acceptance:**
  - `AiService.GetSummary`, `AnalyzeMessage(stream)`, `StreamEnrichments(stream, resume-by-message_id)`, `SuggestReply`, `GetUsage`, `SetPaused`; token-streaming RPCs abort upstream on cancel.
  - `mail ai status|process|summary|reply|retry|pause|resume|cost` verbs.
- **verify:** `cargo nextest run -p rmaild ai_service` (cached get, force analyze stream, enrichment resume)

## 51. Semantic/hybrid retrieval + L2 rerank
- [ ] status
- **depends-on:** 21, 29, 43, 49
- **parallel-safe:** no
- **acceptance:**
  - Semantic + hybrid modes wired into the pipeline; L2 rerank stage over top-K with two backends: local cross-encoder (ONNX, e.g. bge-reranker) on a blocking pool, and Claude listwise rerank (top ~30, structured order + one-line "why", cached by `(query_hash, candidate_ids)`).
  - `search.rerank = off|cross_encoder|claude|auto`; `auto` = cross-encoder interactive, Claude for deep search; degrades to L1 order on error/budget.
- **verify:** `cargo nextest run -p rmail-core rank::l2` (cross-encoder reorder, Claude listwise mock, degrade-on-error, cache key)

## 52. Mailbox RAG `ask_mailbox`
- [ ] status
- **depends-on:** 51, 50
- **parallel-safe:** no
- **acceptance:**
  - `AiService.AskMailbox(stream AskChunk)`: hybrid retrieve → rerank → pack chunks under a token budget → Claude (Sonnet default) with strict "cite message_uid" → stream tokens + citations + retrieval trace; refuses when context doesn't support an answer.
  - `mail ask "<question>"` CLI verb.
- **verify:** `cargo nextest run -p rmaild ask_mailbox` (streamed tokens+citations, grounded-refusal path)

## 53. gRPC→MCP auto-projection
- [ ] status
- **depends-on:** 41, 38
- **parallel-safe:** no
- **acceptance:**
  - MCP tools generated at runtime from the compiled descriptor set + per-RPC annotations (safe/mutating, tool name, arg mapping); each safe RPC → one MCP tool; mutating tools gated by capability-token scope.
  - `mail mcp serve --stdio|--sse`; in-process channel to the daemon (no extra socket hop); a new RPC yields a new tool with zero extra code.
- **verify:** `cargo nextest run -p rmaild mcp::projection` (annotation→tool generation, scope gating, mutating-tool denial under read token)

## 54. MCP tool surface & scope-filtered listing
- [ ] status
- **depends-on:** 53, 50, 52
- **parallel-safe:** no
- **acceptance:**
  - The PRD's core tool set is present and invocable (`search_mail`, `semantic_search`, `read_mail`, `summarize_thread`, `ask_mailbox`, etc.); a read-only token's tool list contains only read tools.
  - MCP `search_mail` returns the exact ranked set the human search returns (same core call).
- **verify:** `cargo nextest run -p rmaild mcp::tools` (tool list under scope, search parity with SearchService)

## 55. Tags subsystem
- [ ] status
- **depends-on:** 6, 39
- **parallel-safe:** yes
- **acceptance:**
  - `tags`/`message_tags` + effective-tags view; `TagService` (Add/Remove/List/Create/BulkTag/SuggestTags/ResolveSuggestion); hierarchy (`/`), colors, per-tag `sync_mode`.
  - `sync_mode=imap` round-trips tag ⇄ IMAP keyword / Gmail `X-GM-LABELS`; `auto` downgrades to local on `NO`; inbound server keywords import as `source='imap'`; `tag:`/`-tag:` operators; bulk tag = single txn + coalesced STORE.
  - `mail tag/untag/tags ...` verbs.
- **verify:** `cargo nextest run -p rmail-core tags::` · `cargo nextest run -p rmaild tag_service`

## 56. Notes subsystem
- [ ] status
- **depends-on:** 6, 18
- **parallel-safe:** yes
- **acceptance:**
  - `notes` + `notes_fts` (trigger-synced); `NoteService` (Add/Edit/Delete/List/WatchNotes); markdown, message/thread target (XOR check), `$EDITOR` flow; `note:`/`has:note` operators feed the lexical retriever.
  - `mail note/notes ...` verbs; last-write-wins on `updated_at`.
- **verify:** `cargo nextest run -p rmail-core notes::` · `cargo nextest run -p rmaild note_service`

## 57. AI auto-tagging + suggestions
- [ ] status
- **depends-on:** 55, 47
- **parallel-safe:** no
- **acceptance:**
  - New mail → low-priority `suggest_tags` job → Haiku structured `[{tag,confidence,rationale}]` → `message_tags(state='pending',source='ai')`; `tag_rules` auto-apply above `min_conf`, rest pending for accept/reject; learns from accept/reject; skips already-user-tagged mail.
  - `mail suggest-tags/accept-tags/reject-tags` verbs; `SuggestTags` streams as Claude responds.
- **verify:** `cargo nextest run -p rmail-core tags::ai` (pending write, auto-apply threshold, accept/reject learning)

## 58. NL smart folders (Claude compile)
- [ ] status
- **depends-on:** 35, 43
- **parallel-safe:** yes
- **acceptance:**
  - `SmartFolderService.Create` accepts a plain-English predicate; Claude compiles it once into a stored hybrid plan (`from:` + FTS + embedding predicate), re-run cheaply each sync; `mail folder new "<nl>"`.
  - Also completes the Stage-0 NL→plan path (`SearchService.CompileQuery`, `mail search --nl`) with a confirmable cached plan.
- **verify:** `cargo nextest run -p rmaild smart_folder:: compile_query::` (NL→plan compile+cache, live membership)

## 59. Fuzzy finder (III-1)
- [ ] status
- **depends-on:** 6, 14, 38
- **parallel-safe:** no
- **acceptance:**
  - `finder_index`/`finder_dirty`/`finder_commands`; triggers write the dirty feed, a ~250ms drain maintains an in-memory `Arc<RwLock<FinderStore>>` (pre-folded `match_blob`, <25 MB for 100k msgs).
  - Skim/fzf subsequence DP scorer with the PRD bonuses/penalties, smart-case, NFKC+ASCII-fold, exact-substring short-circuit, returns `(score,positions)`; blended ranking with recency/unread/importance/frequency/kind weights; scopes + sigils (`>#@/:`).
  - `Finder.Find(stream)` bounded top-K heap flushing descending batches, keystroke cancellation; `mail find` verbs (+`--json`,`--select --action`); MCP `fuzzy_find`.
- **verify:** `cargo nextest run -p rmail-core finder::score finder::rank` · `cargo nextest run -p rmaild finder_service` (streamed batches, cancellation)

## 60. Compose & drafts
- [ ] status
- **depends-on:** 6, 39
- **parallel-safe:** no
- **acceptance:**
  - `ComposeService` draft CRUD; build full RFC 5322 MIME (headers, multipart, correct In-Reply-To/References) from a draft; drafts persist locally.
- **verify:** `cargo nextest run -p rmail-core compose::mime` · `cargo nextest run -p rmaild compose_service`

## 61. Scheduled send & durable outbox (III-5)
- [ ] status
- **depends-on:** 60, 7
- **parallel-safe:** no
- **acceptance:**
  - `outbox` + `followups`; scheduler sleeps until `min(next_due, poll_interval)`, woken by `Notify`/wake-from-sleep/network-up; SMTP via `lettre` (bounded worker pool), appends to IMAP Sent (Bcc stripped).
  - Undo-send = schedule at `now+undo_window`; idempotency via `smtp_message_id` persisted before DATA (at-most-once); transient 4xx→backoff stay `scheduled`, permanent 5xx→`failed`; missed window within `late_tolerance` still sends, else "sent late"; NL time via `chrono` first.
  - `SendScheduler` RPCs + `mail send --at`, `mail undo`, `mail outbox ...`, `mail followup ...`; MCP-originated sends store `origin="ai"` and always get an undo window.
- **verify:** `cargo nextest run -p rmail-core outbox::` (lifecycle, idempotent retry, offline/late tolerance) · `cargo nextest run -p rmaild send_scheduler`

## 62. AI reply drafting
- [ ] status
- **depends-on:** 60, 43, 49
- **parallel-safe:** yes
- **acceptance:**
  - `DraftService.DraftReply(stream)` reads the full local thread + samples of the user's own past replies to that correspondent + a short intent → on-voice reply with correct headers, staged as an editable draft that never auto-sends; `mail reply <id> --ai`.
  - Tone/length rewrite (`RewriteDraft`) producing cyclable, revertible revisions.
- **verify:** `cargo nextest run -p rmaild draft_reply` (streamed draft, headers correct, never auto-sends)

## 63. Pre-send guardian + follow-up/waiting-on tracker
- [ ] status
- **depends-on:** 61, 43
- **parallel-safe:** yes
- **acceptance:**
  - `OutboxService.PreflightCheck` flags "see attached" w/o attachment, wrong/extra recipients, unfilled placeholders, apparent secrets, tone clashes — blocks or warns by severity (auto on send).
  - Follow-up/waiting-on tracker: judge whether a sent message expects a reply, extract the ask, record a deadline, surface an aging waiting-on list, draft a nudge; auto-dismiss on detected reply.
- **verify:** `cargo nextest run -p rmail-core send::preflight followup::` · `cargo nextest run -p rmaild followup_service`

## 64. Feedback logging
- [ ] status
- **depends-on:** 33
- **parallel-safe:** yes
- **acceptance:**
  - `search_log`/`search_impression`/`search_action` populated: impressions with position + serialized feature vector, actions (open/reply/archive/dwell/scroll_past); `SearchService.LogFeedback` RPC; strictly opt-outable (`search.learning=false`), never transmitted.
- **verify:** `cargo nextest run -p rmail-core feedback::` (impression/action logging, opt-out disables writes)

## 65. Offline training + model hot-swap
- [ ] status
- **depends-on:** 64, 31, 37
- **parallel-safe:** no
- **acceptance:**
  - Local nightly/on-demand job turns clicks into position-bias-corrected pairwise labels, trains the L1 GBDT / updates linear weights (optimizing NDCG), evaluates on a held-out slice, and hot-swaps only on a measured NDCG win (`ranker_model.active`), keeping the old model for rollback.
  - Fully local; cold users fall back to the deterministic scorer.
- **verify:** `cargo nextest run -p rmail-core rank::train` (label generation, propensity weighting, guardrail blocks a regression, rollback)

## 66. Rules engine (+ NL synthesis + backtest)
- [ ] status
- **depends-on:** 14, 43, 55
- **parallel-safe:** yes
- **acceptance:**
  - TOML rules mix deterministic predicates (from/subject/header/flags/size regex) with a `claude_is` NL predicate and an actions block (move/label/flag/archive/notify/run-hook/draft-reply); classification cached by `message-id + prompt-hash`; evaluated on each new message.
  - `RuleService.Create/List/Evaluate/Synthesize/Backtest`; NL synthesis prefers cheap deterministic predicates and returns a dry-run over last N days; backtest reports per-message outcomes + Claude explanation per `claude_is`; corrections become few-shot examples.
- **verify:** `cargo nextest run -p rmail-core rules::` · `cargo nextest run -p rmaild rule_service` (eval, dry-run, cache reuse)

## 67. Hooks dispatcher
- [ ] status
- **depends-on:** 14
- **parallel-safe:** yes
- **acceptance:**
  - Config-driven shell commands fire on `on_new_message`/`on_label`/`on_move`/`on_rule_match`/`on_sync_error` with the event JSON on stdin, run in a bounded worker pool with timeouts; `HookService.ListHooks/TestHook`; `mail hook add`.
- **verify:** `cargo nextest run -p rmail-core hooks::` (event→stdin JSON, timeout kill, bounded concurrency)

## 68. Outbound webhooks + Slack forward
- [ ] status
- **depends-on:** 14, 43
- **parallel-safe:** yes
- **acceptance:**
  - Registered endpoints receive HMAC-signed JSON with retries and a persisted delivery queue; payloads can include a Claude summary + extracted fields; `WebhookService.Register/List/ReplayDelivery`.
  - Slack/generic forward action posts a 2-sentence Claude summary + action items + deep link with retry and per-destination templates (`mail forward <id> --to slack:...`).
- **verify:** `cargo nextest run -p rmail-core webhooks::` (HMAC signature, retry/replay, AI-enriched payload)

## 69. Autonomous inbox agent
- [ ] status
- **depends-on:** 66, 38
- **parallel-safe:** yes
- **acceptance:**
  - Scheduled/event-driven bounded agentic loop where Claude calls a constrained, allowlisted toolset (archive/label/snooze/draft-reply/escalate) toward a user policy; dry-run by default; every action logged with its reason; requires an allowlist scope to mutate.
  - `AgentService.RunInboxAgent/GetAgentRunLog`; `mail agent run [--dry-run]`.
- **verify:** `cargo nextest run -p rmaild agent_service` (dry-run makes no mutations, allowlist enforcement, action log)

## 70. AI periodic digest
- [ ] status
- **depends-on:** 49, 43
- **parallel-safe:** yes
- **acceptance:**
  - Scheduled job clusters a window's mail by topic/sender and has Claude produce a ranked markdown briefing (needs-reply/FYI/waiting-on/auto-handled/skipped) with every line linked to source message-ids; `AnalyticsService.GenerateDigest`; `mail digest --since 7d`.
- **verify:** `cargo nextest run -p rmaild digest` (sectioned briefing, every line cites a message-id)

## 71. Response-time & SLA analytics
- [ ] status
- **depends-on:** 9, 10
- **parallel-safe:** yes
- **acceptance:**
  - Pair sent replies to inbound via In-Reply-To/References; compute per-contact/per-mailbox p50/p90 response times + rolling trend; flag where the user is the bottleneck; `AnalyticsService.GetResponseTimes`; `mail stats response-time --by contact`.
- **verify:** `cargo nextest run -p rmail-core analytics::response_time` (pairing, percentile math, bottleneck flag)

## 72. Contact insights, subscriptions & NL analytics
- [ ] status
- **depends-on:** 43, 9
- **parallel-safe:** yes
- **acceptance:**
  - Contact relationship insight (volume/direction/symmetry/cadence/topics → one-paragraph Claude briefing + decay report); newsletter/subscription detector (List-Unsubscribe + heuristics + Claude fallback, read-rate, unsubscribe-candidates + one-click); NL analytics (Claude → safe parameterized read-only SQL over whitelisted views + narrative).
  - `AnalyticsService.GetContactInsight/ListSubscriptions/AskAnalytics`.
- **verify:** `cargo nextest run -p rmail-core analytics::` (subscription classification, SQL whitelist guard rejects writes)

## 73. Structured invoice/receipt & data extraction
- [ ] status
- **depends-on:** 22, 43
- **parallel-safe:** yes
- **acceptance:**
  - Detect invoice/receipt attachments; Claude with a strict schema pulls vendor/number/line-items/totals/currency/due/status into a queryable, CSV-exportable table; general `ExtractStructured` against a JSON schema (invoice/flight/meeting/etc.), validated and stored; `SearchService.SearchEntities`.
  - `mail invoices [--export csv]`, `mail extract <id> --schema invoice`.
- **verify:** `cargo nextest run -p rmail-core extract::invoice extract::structured` (schema-valid rows, CSV export)

## 74. Attachment semantic search & ask-your-attachment
- [ ] status
- **depends-on:** 21, 22, 52
- **parallel-safe:** yes
- **acceptance:**
  - Extracted attachment text chunked+embedded and fused via RRF so "the termination-for-convenience clause" returns the exact attachment + page (`SearchService.SearchAttachments`); `AttachmentService.AskAttachment(stream)` answers a question scoped to one attachment/result-set with page/section citations, refusing unsupported answers.
- **verify:** `cargo nextest run -p rmaild attach_search ask_attachment` (page-cited answer, unsupported refusal)

## 75. Table, calendar/task & link extraction
- [ ] status
- **depends-on:** 22, 43
- **parallel-safe:** yes
- **acceptance:**
  - Table extraction (native from spreadsheets, Claude vision for PDF/image tables) into typed rows with headers + source-cell provenance; calendar/task extraction (message + .ics → normalized events/tasks → .ics / pipe / task webhook, idempotent per message); URL/link extraction + Claude classification (unsubscribe/tracking/meeting/document/CTA) with relevance score + picker.
  - `AttachmentService.ExtractTables`, `ExtractService.ExtractEvents/ExtractTasks`, `LinkService.ExtractLinks`.
- **verify:** `cargo nextest run -p rmail-core extract::tables extract::events extract::links`

## 76. Budget enforcer
- [ ] status
- **depends-on:** 45, 46
- **parallel-safe:** yes
- **acceptance:**
  - Per-account + global daily/monthly token & dollar caps checked before dispatch; soft cap auto-downgrades the model (opus→sonnet→haiku), hard cap blocks; bulk jobs get a separate sub-budget; `AiPolicyService.SetBudget/GetSpend`; `mail ai budget set/status`.
- **verify:** `cargo nextest run -p rmail-core ai::budget` (soft-cap downgrade, hard-cap block, bulk sub-budget)

## 77. Prompt-injection shield
- [ ] status
- **depends-on:** 43, 47
- **parallel-safe:** yes
- **acceptance:**
  - Every body wrapped in untrusted-content delimiters and scanned for injection patterns (hidden text, zero-width chars, "ignore previous instructions"); detected messages flagged and any AI action on them requires confirmation, logged; `AiSafetyService.ScanInjection`; `mail ai scan-injection <id>`.
- **verify:** `cargo nextest run -p rmail-core ai::injection` (pattern/zero-width detection, action-gating on flagged mail)

## 78. Local-only model path
- [ ] status
- **depends-on:** 20, 43
- **parallel-safe:** yes
- **acceptance:**
  - Fully on-device inference route (candle/llama.cpp generation + local embeddings) exposing the same summarize/embed/draft verbs; forced by policy for local-only mail; outputs labeled locally-generated with zero egress; `mail ai provider set <account> local`.
- **verify:** `cargo nextest run -p rmail-core ai::local` (no outbound network under local provider, same verb surface)

## 79. OAuth2 broker (Gmail/Outlook)
- [ ] status
- **depends-on:** 7, 8
- **parallel-safe:** yes
- **acceptance:**
  - Loopback-redirect OAuth2 + PKCE for Google & Microsoft; refresh tokens in Keychain; XOAUTH2 SASL for IMAP/SMTP; refresh-before-expiry; re-consent on revocation; `AccountService.BeginOAuth/CompleteOAuth/RefreshToken`; `mail account login --oauth <provider>`.
- **verify:** `cargo nextest run -p rmail-core oauth::` (PKCE flow against a mock authz server, XOAUTH2 string, refresh)

## 80. Unified inbox + AI account autoconfig
- [ ] status
- **depends-on:** 8, 39, 43
- **parallel-safe:** yes
- **acceptance:**
  - Synthetic unified mailbox merging every account's Inbox into one time-ordered, Message-ID-deduplicated view with actions routed back to the correct account/folder (`MailService.ListUnified`, `mail list --all`).
  - Autoconfig probes ISPDB/SRV/autodiscover and, on a miss, hands domain+MX+probe responses to Claude to infer IMAP/SMTP settings, validates by login, writes a ready TOML block (`mail account add <email>`).
- **verify:** `cargo nextest run -p rmaild unified_inbox` · `cargo nextest run -p rmail-core autoconfig::` (dedup/order, probe→settings, login validation)

## 81. Priority notification engine
- [ ] status
- **depends-on:** 48, 14
- **parallel-safe:** yes
- **acceptance:**
  - On each new-mail event Claude Haiku scores an importance tier + one-line reason; a macOS notification fires only at/above a per-account threshold so newsletters never ping; `NotificationService.ScoreMessage/StreamAlerts`; `mail notify watch`.
- **verify:** `cargo nextest run -p rmail-core notify::` (threshold gating, below-threshold suppressed)

## 82. Multi-format export
- [ ] status
- **depends-on:** 9, 39
- **parallel-safe:** yes
- **acceptance:**
  - Export any query or thread to mbox / Maildir / .eml / JSON, streaming from SQLite and preserving raw RFC822; `--with-ai` batch-attaches Claude summaries + tags to the JSON; `ExportService.Export`; `mail export '<query>' --format mbox -o out.mbox`.
- **verify:** `cargo nextest run -p rmail-core export::` (each format round-trips, raw RFC822 preserved, --with-ai)

## 83. TUI shell (folders / list / preview)
- [ ] status
- **depends-on:** 39, 33
- **parallel-safe:** no
- **acceptance:**
  - `ratatui`/`crossterm` TUI attaching to `rmaild` as a gRPC client (<200 ms startup); folders / message-list / preview layout; message viewer (plain/multipart/quoted-printable/base64/encoded headers, "open HTML in browser"); basic navigation `j/k gg G Enter q ?`; core actions archive/delete/mark/copy/move/reply/forward.
  - UI never blocks on sync/AI (reads local state via gRPC streams).
- **verify:** `cargo nextest run -p rmail-cli tui::model` (headless model/update tests; render smoke test)

## 84. TUI modal vim keymap engine
- [ ] status
- **depends-on:** 83
- **parallel-safe:** no
- **acceptance:**
  - Layered keymap engine (normal/insert/visual, chord sequences), fully rebindable and hot-reloadable via `keys.toml`, mapping to named action ids shared by palette/gRPC/MCP; `ConfigService.GetKeymap/SetBinding`; `mail keys set <chord> <action>`.
- **verify:** `cargo nextest run -p rmail-cli keymap::` (chord resolution, rebind, hot-reload, action-id shared registry)

## 85. TUI overlays (search / finder / AI panel / ask pane / palette / outbox)
- [ ] status
- **depends-on:** 83, 84, 33, 59, 52, 61
- **parallel-safe:** no
- **acceptance:**
  - `/` streaming ranked incremental search (debounced, keystroke-cancel, `~`/`=` prefixes, operator autocomplete, `x` why-panel); Ctrl-P fuzzy finder + NL command palette (`CommandService.ResolveIntent`); collapsible AI panel + streaming Ask pane with citations; Outbox pseudo-folder with undo-toast countdown; AI quick-action menu (`.`).
- **verify:** `cargo nextest run -p rmail-cli tui::overlays` (search stream render, palette resolve, ask-pane citations, outbox undo)

## 86. Supply-chain & release gates
- [ ] status
- **depends-on:** 1
- **parallel-safe:** yes
- **acceptance:**
  - `cargo deny check` and `cargo audit` wired as the final CI gate; `buf breaking` runs against the committed baseline on proto changes; criterion perf benchmarks assert the key budgets (first search hit, full ranked search, fuzzy first batch); macOS release packaging script for `rmaild`+`mail`.
  - Deny/audit/breaking failures fail the build.
- **verify:** `cargo deny check` · `cargo audit` · `buf breaking --against '.git#branch=main'` · `cargo bench -p rmail-core --no-run`
