# rmail — Work Breakdown

Decomposed from `prd.md` (v0.2). Ordered by dependency; the first task scaffolds the
workspace/toolchain so everything after it is verifiable. `/loop` implements the first
unchecked task, reviews it, commits it.

Status legend: `- [ ]` todo · `- [x]` done. Do not reorder IDs — `depends-on` references them.

Crates (established in task 1): `rmail-proto` (generated protos), `rmail-core`
(domain + storage + sync + index + search + ai), `rmaild` (daemon / gRPC server),
`rmail-cli` (the `mail` binary — thin gRPC client).

Global gate (Stop hook enforces on every task): `cargo fmt --all -- --check` ·
`cargo clippy --all-targets --all-features -- -D warnings` · `scripts/docker-test.sh`.
Per-task **verify** lists the *targeted* proof in addition to the global gate.

**Tests only ever run in a container.** The per-task `verify` lines below are written
as `cargo nextest run …` for readability; run them as
`scripts/docker-test.sh <same args>` — the wrapper passes cargo arguments straight
through. Do not run `cargo test`/`cargo nextest` on the host.

**A bare filter matches a test's *name*, never its binary.** `cargo nextest run -p
rmaild mail_service` selected **zero** tests for the whole life of this file: it looks
like it names the `rmaild/tests/mail_service.rs` binary, but nextest matches the string
against test names, and none of that binary's 22 tests has `mail_service` in its name.
A whole integration suite can be "verified" by a command that runs nothing. To select a
test *binary*, use `--test <name>` (or `-E 'binary(<name>)'`); a bare word is only for
matching a module path or a name prefix, as in `-p rmail-core config::`. Every line
below was swept against `cargo nextest list` and now selects what it claims to.

**`-p` does not narrow anything either.** `scripts/docker-test.sh` always injects
`--workspace`, and cargo ignores `--package` when `--workspace` is present — so
`scripts/docker-test.sh -p rmail-core parity::` applies `parity::` to the whole
workspace, not to `rmail-core`. The `-p` in the lines below documents where a task's
tests live; it is the filter (or `--test`) that does the selecting.

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
- **verify:** `cargo build --release` · `buf lint` · `cargo nextest run -p rmaild --test health` · `cargo fmt --all -- --check` · `cargo clippy --all-targets --all-features -- -D warnings`

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
- **verify:** `cargo nextest run -p rmaild --test sync_service` (in-process server: trigger sync, observe streamed events, resume)

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
- [x] status
- **depends-on:** 22
- **parallel-safe:** yes
- **acceptance:**
  - Image attachments and text-less PDFs routed to OCR (Apple Vision default, Tesseract fallback) producing searchable text + bounding boxes; native-vs-OCR provenance and confidence recorded; opt-in via config.
- **verify:** `cargo nextest run -p rmail-core attach::ocr` (fixture image → text, provenance flag)

## 24. IndexService gRPC + `mail index` CLI
- [x] status
- **depends-on:** 16, 18, 19, 21
- **parallel-safe:** no
- **acceptance:**
  - `IndexService.Status/Reindex(stream)` plus `mail index status|run|start|stop|reindex|rebuild|verify|gc|embed --backfill` and `mail entities <kind>`.
  - `status` reports per-kind coverage %, queue depth, model/dim, and lag; `verify` detects state/content-hash drift; `gc` vacuums orphans.
- **verify:** `cargo nextest run -p rmaild --test index_service` (status coverage, reindex stream, verify drift)

## 25. Query understanding — operator parser & grammar
- [x] status
- **depends-on:** 2, 3
- **parallel-safe:** no
- **acceptance:**
  - Parser for the operator grammar (`from:`,`to:`,`cc:`,`subject:`,`body:`,`has:`,`filename:`,`larger:`/`smaller:`,`before:`/`after:`/`on:`/`date:`,`is:`,`tag:`,`note:`,`in:`,`account:`,`thread:`,`ai:`, quotes, `-` negation, `~`/`=` mode sigils).
  - Operators become hard filters (WHERE); free text becomes ranked terms/phrases; unknown `key:value` degrades to free text (never an error).
- **verify:** `cargo nextest run -p rmail-core query::parse` (each operator, negation, phrase, unknown-key passthrough)

## 26. Query understanding — QueryPlan assembly
- [x] status
- **depends-on:** 25, 19
- **parallel-safe:** no
- **acceptance:**
  - Deterministic pipeline producing `QueryPlan{hard_filters, lexical_terms, phrases, expansions, query_vector?, entities, intent, sort, scope}`.
  - Intent classification (navigational/exploratory/lookup) via a cheap local feature logistic; SymSpell/trigram spell-fix against corpus vocabulary; alias/contact resolution to soft boosts; PMI synonym expansion.
  - Query embedded once (local) for the dense retriever; Claude NL-compile is a stubbed fallback flag (wired in task 43/58).
- **verify:** `cargo nextest run -p rmail-core query::plan` (intent labels, spellfix, expansion, plan shape)

## 27. Lexical BM25 retriever
- [x] status
- **depends-on:** 18, 25
- **parallel-safe:** no
- **acceptance:**
  - Retriever over `fts_messages` returning top-N with source-local BM25 score+rank; honors hard filters as a candidate mask; phrase/`NEAR` proximity and an unquoted proximity bonus.
- **verify:** `cargo nextest run -p rmail-core retrieve::lexical`

## 28. Candidate generation — remaining retrievers
- [x] status
- **depends-on:** 19, 21, 26, 27
- **parallel-safe:** no
- **acceptance:**
  - Dense kNN (chunk→message, keeping max/mean similarity), fuzzy (nucleo + trigram), entity match, structured filter (hard gate), prefix/autocomplete, and recency-prior retrievers each return top-N with source score+rank.
  - All run concurrently on a bounded pool; each is individually skippable (config/degradation); a query-generation token cancels superseded scans.
- **verify:** `cargo nextest run -p rmail-core retrieve::` (each retriever + parallel fan-out + cancellation)

## 29. Fusion & dedup (RRF + SimHash)
- [x] status
- **depends-on:** 28
- **parallel-safe:** no
- **acceptance:**
  - Weighted RRF (`k=60`, intent-dependent per-source weights) over all sources; chunk→message and optional message→thread collapse; SimHash near-duplicate collapse.
  - Linear-blend fusion available via `fusion="linear"`; output carries every source's rank+score for downstream features.
- **verify:** `cargo nextest run -p rmail-core fuse::` (RRF math, intent weights, dedup, near-dup collapse)

## 30. Feature extraction
- [x] status
- **depends-on:** 29
- **parallel-safe:** no
- **acceptance:**
  - Per-candidate feature vector (textual/semantic/personal/temporal/status/structural/global groups) computed cheaply from local DB + fused metadata; deterministic and serializable for replay.
- **verify:** `cargo nextest run -p rmail-core features::` (vector completeness, serialization round-trip)

## 31. L1 deterministic ranker
- [x] status
- **depends-on:** 30
- **parallel-safe:** no
- **acceptance:**
  - Cold-start linear scorer with the PRD weights (TOML-overridable) scoring all fused candidates and keeping top-K (50); pure-Rust microsecond inference; newsletter/automated down-weight gated by intent.
  - Pluggable behind a `Ranker` trait so a learned model (task 65) can hot-swap.
- **verify:** `cargo nextest run -p rmail-core rank::l1` (weighted score, top-K cut, intent gating)

## 32. Diversify & present
- [x] status
- **depends-on:** 31
- **parallel-safe:** no
- **acceptance:**
  - MMR (λ=0.7) for exploratory intent, disabled for navigational; thread grouping with `+N` affordance; near-dup collapse chip; snippet extraction + query-term highlight (FTS5 `snippet()` / best chunk).
  - Results emitted best-first in score-ordered batches (streaming-ready).
- **verify:** `cargo nextest run -p rmail-core present::` (MMR diversity, snippet/highlight, streaming order)

## 33. SearchService gRPC (streaming) + Explain
- [x] status
- **depends-on:** 32
- **parallel-safe:** no
- **acceptance:**
  - `SearchService.Search(stream SearchHit)`, `Semantic`, and `Explain` wired end-to-end through the pipeline; first hit reaches the client fast; a fresh request cancels the prior stream (generation token).
  - `SearchHit` carries score, highlighted snippet, `sources`, and (when `explain`) a `RankExplanation` of top feature contributions + matched spans.
- **verify:** `cargo nextest run -p rmaild --test search_service` · `cargo nextest run -p rmaild search_service::` (streamed hits, cancellation, explain block)

## 34. Search CLI verbs
- [x] status
- **depends-on:** 33
- **parallel-safe:** yes
- **acceptance:**
  - `mail search "<q>"`, `--explore`, `--explain`, `--json`, `~`/`=` prefixes, and `mail similar <id>` implemented as gRPC-client verbs; `--json` emits the PRD item schema (uid, subject, score, snippet, sources, why).
- **verify:** `cargo nextest run -p rmail-cli --test search_cli` · `cargo nextest run -p rmail-cli search_cli::` (json schema, flags map to request fields)

## 35. Saved searches & deterministic smart folders
- [x] status
- **depends-on:** 33, 6
- **parallel-safe:** yes
- **acceptance:**
  - Named saved searches persisted and re-runnable through the full pipeline; deterministic smart folders (operator-DSL predicate) re-evaluated on each sync so membership stays live without moving server mail; can trigger auto-tag/notify on new matches. (NL-compiled smart folders land in task 58.)
- **verify:** `cargo nextest run -p rmail-core saved_search:: smart_folder::` · `cargo nextest run -p rmaild --test saved_search_service`

## 36. Query/embedding/result caching & incrementality
- [x] status
- **depends-on:** 33
- **parallel-safe:** yes
- **acceptance:**
  - Query-plan cache (normalized hash), embedding cache (persist doc/query vectors, re-embed on content_hash change), and result cache keyed by `(query, filter, corpus_version)` invalidated on corpus bump/ranker change; freshly-synced mail bypasses the result cache.
- **as built:** two of the three caches already existed and were left alone —
  `query_plan_cache` (V47, task 58) is the query-plan cache, and
  `chunk_embeddings.content_hash` (task 24) is the *document* half of the
  embedding cache. This task added the **query** half (`embedding_cache`, via a
  `CachingEmbedder` decorator wrapping only the query-embedding paths — the
  indexer keeps the raw embedder so chunk vectors do not evict query vectors),
  the **result cache** (`search_result_cache`), and the `corpus_version` both
  are keyed against. That version is a single global counter maintained by SQL
  **triggers** on `messages`/`flags`/`message_tags`/`index_content`, not by a
  `bump()` any write path has to remember; global rather than per-account
  because over-invalidation costs a recomputed search while
  under-invalidation is a wrong one. Nothing is invalidated by deleting rows:
  the corpus version and a digest of the whole `[search]` table plus the
  embedding model are *inside* each result key, so new mail or any retuned
  knob moves the key. `ResultCache::lookup` returns Hit/Miss(Lease)/Bypass —
  a bypass (fresh corpus, or an unreadable version) yields no lease, and
  `store` re-reads the version and declines if mail landed mid-search.
  Operator surface: cache counters on `IndexService.Status`, sweep plus opt-in
  `purge_search_caches` on `IndexService.Gc`, `mail index gc --purge-caches`.
- **verify:** `cargo nextest run -p rmail-core --lib cache::` (32 tests: hit/miss counted with a counting embedder, corpus-version/ranker/model invalidation, fresh-mail bypass, mid-search corpus move, LRU and TTL bounds) · `cargo nextest run -p rmaild --test index_service status_reports_the_search_caches` (operator surface end to end)

## 37. Evaluation harness + CI regression guard
- [x] status
- **depends-on:** 33
- **parallel-safe:** yes
- **acceptance:**
  - Versioned golden set `(query, judged-relevant ids)`; `mail search eval` reports NDCG@10, MRR, Recall@50, P@3; offline replay/shadow scoring over logged impressions.
  - CI job runs the golden set on a fixture corpus and fails the build on an NDCG@10 drop below threshold.
- **verify:** `cargo nextest run -p rmail-core eval::` · `cargo nextest run -p rmaild --test eval_service` · `mail search eval` on the fixture corpus meets threshold

## 38. Capability tokens & auth interceptor
- [x] status
- **depends-on:** 6, 3
- **parallel-safe:** no
- **acceptance:**
  - `api_tokens` (argon2id hashes, scopes, expiry, revoked); `AdminService.MintToken/RevokeToken/ListTokens`; `mail token create/list/revoke`.
  - tonic interceptor enforces per-method scope (`mail.read`,`mail.write`,`mail.send`,`ai.invoke`,`ai.spend:<usd>`,`mailbox:<name>`,`automation`,`admin`); Unix-socket peer-uid (`SO_PEERCRED`) grants implicit admin; TCP requires Bearer token (constant-time verify) or mTLS.
  - A read-only token is physically denied `Send`/`Delete`.
- **verify:** `cargo nextest run -p rmaild auth::` · `cargo nextest run -p rmaild --test admin_service` (scope allow/deny matrix, peer-uid path, revoked token rejected)

## 39. MailService
- [x] status
- **depends-on:** 9, 10, 14, 38
- **parallel-safe:** no
- **acceptance:**
  - `List(stream)`, `Get`, `GetThread`, `Move`, `Copy`, `SetFlags`, `Delete`, `GetAttachment(stream)`, `WatchEvents(stream)` implemented over core services with correct scopes; attachments chunk-streamed within the 16 MiB frame cap.
  - Mutations reflect to IMAP (flags/move) and emit `events`.
- **verify:** `cargo nextest run -p rmaild --test mail_service` (CRUD, threaded get, watch stream, attachment chunking)

## 40. Idempotency, pagination & error-model hardening
- [x] status
- **carried over from task 55:** tags and notes key off the stable `messages.id`, and `prd.md` promises both follow a message across a move ("Message move → tags/notes follow stable `messages.id`, only the keyword re-`STORE`d"). `MailService.Move` does not keep that promise: `mail::MailStore::move_message` deletes the local row and lets the next sync re-insert the message under a **new** id, so its tags and notes are silently orphaned. Fix the move path to preserve the row identity (or re-point the annotations), and prove it with a test that tags a message, moves it, and asserts the tag survives.
- **depends-on:** 39
- **parallel-safe:** no
- **acceptance:**
  - `idempotency_keys` table; mutating RPCs accept an `idempotency_key` — same key+hash replays the cached response, differing payload → `ALREADY_EXISTS`.
  - Server-capped `page_size` (≤500) + opaque `page_token` on list RPCs; all error paths carry stable `ErrorInfo.reason`.
- **verify:** `cargo nextest run -p rmaild idempotency:: pagination::`

## 41. Feature-parity command enum + CI drift check
- [x] status
- **depends-on:** 39, 33
- **parallel-safe:** no
- **acceptance:**
  - A single internal command enum backs CLI/TUI/gRPC; a test enumerates every core command and asserts a corresponding RPC exists.
  - CI job fails if any core command lacks an RPC (no CLI/gRPC/MCP feature drift).
- **verify:** `cargo nextest run -p rmail-core parity::` · `cargo nextest run -p rmaild auth::methods::` (every command → RPC; missing mapping fails; effect/scope agree)

## 42. CLI as gRPC client: structured output & generic call
- [x] status
- **depends-on:** 38, 40
- **parallel-safe:** no
- **acceptance:**
  - Global `--format {table,json,ndjson}` on every command with stable serde schemas and stable exit codes; streaming commands emit ndjson mirroring gRPC frames.
  - `rmail daemon start|status|stop`, `rmail api ping|reflect|call <Method> <json>`, and global flags (`--socket`,`--addr`,`--token`,`--tls-*`,`--insecure`,`--deadline`); daemon auto-start or `FAILED_PRECONDITION`.
- **verify:** `cargo nextest run -p rmail-cli format:: api_call::` (format stability, generic call via reflection, exit codes)
- **notes:** `--format` is accepted on every verb and never falls through to a
  table: a verb with no structured rendering *refuses* with exit 12 and names
  `mail api call <Method> '<json>'`, which prints any RPC's response as proto
  JSON today. 16 verbs render structured output directly (`format::STRUCTURED`);
  the remaining ~88 are declared in `format::NO_CURATED_SCHEMA` with the reason,
  and `format::tests::every_cli_verb_declares_how_it_answers_format_json` fails
  by name for any verb in neither list — so the gap is enforced and visible,
  not silent. **Follow-up:** curate a per-verb schema for those 88 (one
  `format::emit_response(Command::X, &response)?` per call site) and move each
  path from `NO_CURATED_SCHEMA` to `STRUCTURED`.
  `--tls-*` is implemented (tonic's `tls-webpki-roots`) but cannot be exercised
  end to end: `rmaild` serves no TCP listener yet, so the integration test
  reaches TCP over `--insecure`. Daemon lifecycle uses explicit start
  (`FAILED_PRECONDITION` elsewhere), never auto-start.
  **Flag rename:** `mail export`'s archive format is now `--archive-format`
  (`-f` unchanged). It cannot share the id `format` with the global flag —
  `clap` merges a global and a subcommand argument of the same id by
  value-source precedence and writes the winner into both, so
  `RMAIL_FORMAT=json mail export -o backup.mbox` wrote a JSON archive into a
  `.mbox` file with no diagnostic. prd.md item 63's `--format mbox` spelling is
  superseded by task 42's global flag (prd.md item 37); see `export_cli`'s
  module docs.

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
- [x] status
- **depends-on:** 43
- **parallel-safe:** no
- **acceptance:**
  - Mandatory pre-flight over every body/thread before any Claude call: reversibly tokenizes emails, phones, cards (Luhn), addresses, secrets, names in memory; re-hydrates the model response so the user sees real values but the API never receives raw PII.
  - Empty-after-redaction short-circuits to `redacted_skip`; `redact_preview` surface exposes what would be sent.
- **verify:** `cargo nextest run -p rmail-core ai::redact` (tokenize/rehydrate round-trip, Luhn, no raw PII in outbound payload)

## 45. AI audit ledger + usage/cost accounting
- [x] status
- **depends-on:** 6, 43
- **parallel-safe:** yes
- **acceptance:**
  - Append-only ledger recording every Claude request (timestamp, ids, model, tokens, cost, redaction level, latency, SHA-256 of the exact payload sent); `ai_usage` day rollups; every AI artifact links to its ledger entry.
  - `AuditService.QueryAiCalls/ExportLedger`.
- **verify:** `cargo nextest run -p rmail-core ai::audit` · `cargo nextest run -p rmaild --test audit_service` (append-only invariant, payload hash, cost rollup)

## 46. AI policy & data-residency engine
- [x] status
- **depends-on:** 2, 6
- **parallel-safe:** yes
- **acceptance:**
  - Declarative per-account/folder/pattern `allowed | local-only | forbidden` + residency tag; every AI path consults it first; forbidden folders are invisible to AI features; every resolution is logged and `explain`-able.
- **verify:** `cargo nextest run -p rmail-core ai::policy` (allow/deny/local-only resolution, forbidden invisibility, explain trace)

## 47. AI queue & worker pool
- [x] status
- **depends-on:** 16, 43, 44, 45, 46
- **parallel-safe:** no
- **acceptance:**
  - Persistent `ai_queue` (dedup `UNIQUE(message_id,pass)`), lease model with expiry reaping, `Semaphore(max_concurrency)` + token-bucket RPM limiter; cost gate against `ai_usage[today]` applying `on_cap` (pause/triage_only/drop).
  - Batch mode flips to the Message Batches API when depth ≥ threshold (`custom_id = message_id`, 50% cost); offline rows stay `pending` and drain on reconnect; provider 429/5xx → backoff then `dead`, `mail ai retry --failed` requeues.
- **verify:** `cargo nextest run -p rmail-core ai::queue` (dedup, lease reclaim, RPM/cost gate, batch flip, retry→dead)

## 48. Triage pass (Haiku)
- [x] status
- **depends-on:** 47
- **parallel-safe:** no
- **acceptance:**
  - Every newly synced message runs one Haiku structured-JSON call → category, priority, `needs_reply`, sentiment, suggested tags, `tl_dr` written to `ai_summaries`; `ai_fts` FTS index over AI fields; `ai:` search operators functional.
- **verify:** `cargo nextest run -p rmail-core ai::triage` (schema-valid output, ai_fts populated, `ai:needs-reply` filter)

## 49. Deep pass + thread-aware summary
- [x] status
- **depends-on:** 48
- **parallel-safe:** no
- **acceptance:**
  - Conditional deep pass (Opus/Sonnet) when triage flags priority≥high / needs_reply / allowlisted category: summary, key_points, todos, entities/dates/amounts, suggested_reply, incremental thread summary folding prior `ai_summaries.summary` for the thread.
  - Enrichments feed the lexical + semantic indexes.
- **verify:** `cargo nextest run -p rmail-core ai::deep` (gating logic, thread rollup incrementality, index feed)

## 50. AiService gRPC + streaming RPCs
- [x] status
- **depends-on:** 48, 49
- **parallel-safe:** no
- **acceptance:**
  - `AiService.GetSummary`, `AnalyzeMessage(stream)`, `StreamEnrichments(stream, resume-by-message_id)`, `SuggestReply`, `GetUsage`, `SetPaused`; token-streaming RPCs abort upstream on cancel.
  - `mail ai status|process|summary|reply|retry|pause|resume|cost` verbs.
  - **Per-thread deep-pass serialization — carried over from task 49.** `build_request` reads a thread's prior rollup *before* the concurrency semaphore and before the provider call, so two messages of the same thread leased in one dispatch cycle both read the same prior state and the last writer overwrites the other's contribution to `ai_summaries.thread_summary`. No content is lost from the mailbox (each row keeps its own `summary`), only from the rollup. It cannot be fixed inside the handler — a process-local lock would not bind a second daemon or worker pool — so the queue must serialize dispatch per thread, e.g. capping concurrent `"deep"` leases to one per thread per cycle. This bites hardest on the batch path, which is exactly where a thread most often has several messages queued at once (backlog, initial sync).
  - **Daemon dispatch loop — carried over from task 48.** Task 48 built the triage `PassHandler` and task 47 the queue, but *nothing enqueues a triage job when a message syncs*: the PRD's "every newly synced message runs one Haiku call" has no wiring. This task owns it — subscribe the daemon to the sync event bus (`rmail-core/src/events/`), enqueue via `AiQueue::enqueue`, and run `AiWorkerPool::dispatch_pending` / `BatchCoordinator::maybe_submit`+`poll` on a schedule. Without this the whole AI pipeline is inert in production however green its unit tests are, so cover it with a test that syncs a message and asserts a job appears.
- **verify:** `cargo nextest run -p rmaild --test ai_service` (cached get, force analyze stream, enrichment resume)

## 51. Semantic/hybrid retrieval + L2 rerank
- [x] status
- **depends-on:** 21, 29, 43, 49
- **parallel-safe:** no
- **acceptance:**
  - Semantic + hybrid modes wired into the pipeline; L2 rerank stage over top-K with two backends: local cross-encoder (ONNX, e.g. bge-reranker) on a blocking pool, and Claude listwise rerank (top ~30, structured order + one-line "why", cached by `(query_hash, candidate_ids)`).
  - `search.rerank = off|cross_encoder|claude|auto`; `auto` = cross-encoder interactive, Claude for deep search; degrades to L1 order on error/budget.
- **verify:** `cargo nextest run -p rmail-core rank::l2` (cross-encoder reorder, Claude listwise mock, degrade-on-error, cache key)

## 52. Mailbox RAG `ask_mailbox`
- [x] status
- **depends-on:** 51, 50
- **parallel-safe:** no
- **acceptance:**
  - `AiService.AskMailbox(stream AskChunk)`: hybrid retrieve → rerank → pack chunks under a token budget → Claude (Sonnet default) with strict "cite message_uid" → stream tokens + citations + retrieval trace; refuses when context doesn't support an answer.
  - `mail ask "<question>"` CLI verb.
- **verify:** `cargo nextest run -p rmaild ask_mailbox` (streamed tokens+citations, grounded-refusal path)

## 53. gRPC→MCP auto-projection
- [x] status
- **depends-on:** 41, 38
- **parallel-safe:** no
- **acceptance:**
  - MCP tools generated at runtime from the compiled descriptor set + per-RPC annotations (safe/mutating, tool name, arg mapping); each safe RPC → one MCP tool; mutating tools gated by capability-token scope.
  - `mail mcp serve --stdio|--sse`; in-process channel to the daemon (no extra socket hop); a new RPC yields a new tool with zero extra code.
- **verify:** `cargo nextest run -p rmaild mcp::projection` (annotation→tool generation, scope gating, mutating-tool denial under read token)

## 54. MCP tool surface & scope-filtered listing
- [x] status
- **depends-on:** 53, 50, 52
- **parallel-safe:** no
- **acceptance:**
  - The PRD's core tool set is present and invocable (`search_mail`, `semantic_search`, `read_mail`, `summarize_thread`, `ask_mailbox`, etc.); a read-only token's tool list contains only read tools.
  - MCP `search_mail` returns the exact ranked set the human search returns (same core call).
- **verify:** `cargo nextest run -p rmaild mcp::tools` (tool list under scope, search parity with SearchService)

## 55. Tags subsystem
- [x] status
- **depends-on:** 6, 39
- **parallel-safe:** yes
- **acceptance:**
  - `tags`/`message_tags` + effective-tags view; `TagService` (Add/Remove/List/Create/BulkTag/SuggestTags/ResolveSuggestion); hierarchy (`/`), colors, per-tag `sync_mode`.
  - `sync_mode=imap` round-trips tag ⇄ IMAP keyword / Gmail `X-GM-LABELS`; `auto` downgrades to local on `NO`; inbound server keywords import as `source='imap'`; `tag:`/`-tag:` operators; bulk tag = single txn + coalesced STORE.
  - `mail tag/untag/tags ...` verbs.
- **verify:** `cargo nextest run -p rmail-core tags::` · `cargo nextest run -p rmaild --test tag_service`

## 56. Notes subsystem
- [x] status
- **depends-on:** 6, 18
- **parallel-safe:** yes
- **acceptance:**
  - `notes` + `notes_fts` (trigger-synced); `NoteService` (Add/Edit/Delete/List/WatchNotes); markdown, message/thread target (XOR check), `$EDITOR` flow; `note:`/`has:note` operators feed the lexical retriever.
  - `mail note/notes ...` verbs; last-write-wins on `updated_at`.
- **verify:** `cargo nextest run -p rmail-core notes::` · `cargo nextest run -p rmaild --test note_service`

## 57. AI auto-tagging + suggestions
- [x] status
- **depends-on:** 55, 47
- **parallel-safe:** no
- **acceptance:**
  - New mail → low-priority `suggest_tags` job → Haiku structured `[{tag,confidence,rationale}]` → `message_tags(state='pending',source='ai')`; `tag_rules` auto-apply above `min_conf`, rest pending for accept/reject; learns from accept/reject; skips already-user-tagged mail.
  - `mail suggest-tags/accept-tags/reject-tags` verbs; `SuggestTags` streams as Claude responds.
- **verify:** `cargo nextest run -p rmail-core tags::ai` (pending write, auto-apply threshold, accept/reject learning)
- **closed at merge:** `tag_rules` shipped with no RPC/CLI/MCP surface, so `TagStore::set_tag_rule`/`list_tag_rules` had no caller outside `rmail-core` and an operator could not create the `mode='auto'` rule auto-apply requires — every suggestion pended. Added `TagService.SetTagRule`/`ListTagRules` (additive to v1), the matching `parity` variants and `auth::methods` rows (`mail.write` to set, `mail.read` to list), and `mail tag-rules list|set`. Covered by `rmaild --test tag_service`: reachable through the daemon, upserts on (account, name) rather than accumulating, an unspecified mode resolves to `suggest` not `auto`, and an out-of-range floor is refused. Deleting a rule is `--disabled`; a hard delete has no caller yet.

## 58. NL smart folders (Claude compile)
- [x] status
- **depends-on:** 35, 43
- **parallel-safe:** yes
- **acceptance:**
  - `SmartFolderService.Create` accepts a plain-English predicate; Claude compiles it once into a stored hybrid plan (`from:` + FTS + embedding predicate), re-run cheaply each sync; `mail folder new "<nl>"`.
  - Also completes the Stage-0 NL→plan path (`SearchService.CompileQuery`, `mail search --nl`) with a confirmable cached plan.
- **verify:** `cargo nextest run -p rmaild smart_folder:: compile_query::` (NL→plan compile+cache, live membership)
- **notes:** one compiler serves both halves (`rmail_core::query::compile`), cached per account by normalized query hash (V47 `query_plan_cache`), so `mail search --nl` and `mail folder new` share a compile. Membership is `<hard filters> AND (<FTS> OR <embedding kNN ≥ floor>)` — `rmail_core::smart_folder::membership`; the query vector is frozen at create time, so an evaluation makes no provider or embedder call. `CompileSmartFolder` is a separate RPC from `CreateSmartFolder` so `ai.invoke` is not forced onto deterministic folders. Model output round-trips through `query::parse` and `validate_hybrid_predicate` before storage; an empty arm compiles to `0`, never a dropped clause.

## 59. Fuzzy finder (III-1)
- [x] status
- **depends-on:** 6, 14, 38
- **parallel-safe:** no
- **acceptance:**
  - `finder_index`/`finder_dirty`/`finder_commands`; triggers write the dirty feed, a ~250ms drain maintains an in-memory `Arc<RwLock<FinderStore>>` (pre-folded `match_blob`, <25 MB for 100k msgs).
  - Skim/fzf subsequence DP scorer with the PRD bonuses/penalties, smart-case, NFKC+ASCII-fold, exact-substring short-circuit, returns `(score,positions)`; blended ranking with recency/unread/importance/frequency/kind weights; scopes + sigils (`>#@/:`).
  - `Finder.Find(stream)` bounded top-K heap flushing descending batches, keystroke cancellation; `mail find` verbs (+`--json`,`--select --action`); MCP `fuzzy_find`.
- **verify:** `cargo nextest run -p rmail-core finder::score finder::rank` · `cargo nextest run -p rmaild --test finder_service` (streamed batches, cancellation)

## 60. Compose & drafts
- [x] status
- **depends-on:** 6, 39
- **parallel-safe:** no
- **acceptance:**
  - `ComposeService` draft CRUD; build full RFC 5322 MIME (headers, multipart, correct In-Reply-To/References) from a draft; drafts persist locally.
- **verify:** `cargo nextest run -p rmail-core compose::mime` · `cargo nextest run -p rmaild --test compose_service`

## 61. Scheduled send & durable outbox (III-5)
- [x] status
- **depends-on:** 60, 7
- **parallel-safe:** no
- **acceptance:**
  - `outbox` + `followups`; scheduler sleeps until `min(next_due, poll_interval)`, woken by `Notify`/wake-from-sleep/network-up; SMTP via `lettre` (bounded worker pool), appends to IMAP Sent (Bcc stripped).
  - Undo-send = schedule at `now+undo_window`; idempotency via `smtp_message_id` persisted before DATA (at-most-once); transient 4xx→backoff stay `scheduled`, permanent 5xx→`failed`; missed window within `late_tolerance` still sends, else "sent late"; NL time via `chrono` first.
  - `SendScheduler` RPCs + `mail send --at`, `mail undo`, `mail outbox ...`, `mail followup ...`; MCP-originated sends store `origin="ai"` and always get an undo window.
- **verify:** `cargo nextest run -p rmail-core outbox::` (lifecycle, idempotent retry, offline/late tolerance) · `cargo nextest run -p rmaild --test send_scheduler`

## 62. AI reply drafting
- [x] status
- **depends-on:** 60, 43, 49
- **parallel-safe:** yes
- **acceptance:**
  - `DraftService.DraftReply(stream)` reads the full local thread + samples of the user's own past replies to that correspondent + a short intent → on-voice reply with correct headers, staged as an editable draft that never auto-sends; `mail reply <id> --ai`.
  - Tone/length rewrite (`RewriteDraft`) producing cyclable, revertible revisions.
- **verify:** `cargo nextest run -p rmaild --test draft_reply` (streamed draft, headers correct, never auto-sends)

## 63. Pre-send guardian + follow-up/waiting-on tracker
- [x] status
- **depends-on:** 61, 43
- **parallel-safe:** yes
- **acceptance:**
  - `OutboxService.PreflightCheck` flags "see attached" w/o attachment, wrong/extra recipients, unfilled placeholders, apparent secrets, tone clashes — blocks or warns by severity (auto on send).
  - Follow-up/waiting-on tracker: judge whether a sent message expects a reply, extract the ask, record a deadline, surface an aging waiting-on list, draft a nudge; auto-dismiss on detected reply.
- **verify:** `cargo nextest run -p rmail-core send::preflight followup::` · `cargo nextest run -p rmaild --test followup_service`

## 64. Feedback logging
- [x] status
- **depends-on:** 33
- **parallel-safe:** yes
- **acceptance:**
  - `search_log`/`search_impression`/`search_action` populated: impressions with position + serialized feature vector, actions (open/reply/archive/dwell/scroll_past); `SearchService.LogFeedback` RPC; strictly opt-outable (`search.learning=false`), never transmitted.
- **verify:** `cargo nextest run -p rmail-core feedback::` (impression/action logging, opt-out disables writes)

## 65. Offline training + model hot-swap
- [x] status
- **depends-on:** 64, 31, 37
- **parallel-safe:** no
- **acceptance:**
  - Local nightly/on-demand job turns clicks into position-bias-corrected pairwise labels, trains the L1 GBDT / updates linear weights (optimizing NDCG), evaluates on a held-out slice, and hot-swaps only on a measured NDCG win (`ranker_model.active`), keeping the old model for rollback.
  - Fully local; cold users fall back to the deterministic scorer.
- **verify:** `cargo nextest run -p rmail-core rank::train` (label generation, propensity weighting, guardrail blocks a regression, rollback) · `cargo nextest run -p rmaild --test search_service train` (train/list/rollback over gRPC)

## 66. Rules engine (+ NL synthesis + backtest)
- [x] status
- **depends-on:** 14, 43, 55
- **parallel-safe:** yes
- **acceptance:**
  - TOML rules mix deterministic predicates (from/subject/header/flags/size regex) with a `claude_is` NL predicate and an actions block (move/label/flag/archive/notify/run-hook/draft-reply); classification cached by `message-id + prompt-hash`; evaluated on each new message.
  - `RuleService.Create/List/Evaluate/Synthesize/Backtest`; NL synthesis prefers cheap deterministic predicates and returns a dry-run over last N days; backtest reports per-message outcomes + Claude explanation per `claude_is`; corrections become few-shot examples.
- **verify:** `cargo nextest run -p rmail-core rules::` · `cargo nextest run -p rmaild --test rule_service` (eval, dry-run, cache reuse)

## 67. Hooks dispatcher
- [x] status
- **depends-on:** 14
- **parallel-safe:** yes
- **acceptance:**
  - Config-driven shell commands fire on `on_new_message`/`on_label`/`on_move`/`on_rule_match`/`on_sync_error` with the event JSON on stdin, run in a bounded worker pool with timeouts; `HookService.ListHooks/TestHook`; `mail hook add`.
- **verify:** `cargo nextest run -p rmail-core hooks::` · `cargo nextest run -p rmaild --test hook_service` (event→stdin JSON, timeout kill, bounded concurrency)

## 68. Outbound webhooks + Slack forward
- [x] status
- **depends-on:** 14, 43
- **parallel-safe:** yes
- **acceptance:**
  - Registered endpoints receive HMAC-signed JSON with retries and a persisted delivery queue; payloads can include a Claude summary + extracted fields; `WebhookService.Register/List/ReplayDelivery`.
  - Slack/generic forward action posts a 2-sentence Claude summary + action items + deep link with retry and per-destination templates (`mail forward <id> --to slack:...`).
- **verify:** `cargo nextest run -p rmail-core webhooks::` (HMAC signature, retry/replay, AI-enriched payload)

## 69. Autonomous inbox agent
- [x] status
- **depends-on:** 66, 38
- **parallel-safe:** yes
- **acceptance:**
  - Scheduled/event-driven bounded agentic loop where Claude calls a constrained, allowlisted toolset (archive/label/snooze/draft-reply/escalate) toward a user policy; dry-run by default; every action logged with its reason; requires an allowlist scope to mutate.
  - `AgentService.RunInboxAgent/GetAgentRunLog`; `mail agent run [--dry-run]`.
- **verify:** `cargo nextest run -p rmaild --test agent_service` (dry-run makes no mutations, allowlist enforcement, action log) · `cargo nextest run -p rmail-core --lib agent::` (the closed vocabulary, the shield, the three bounds, the log outliving an archive)
- **note:** *the model never chooses what to look at.* `agent::store::candidates` is an ordinary query over one account and one mailbox, so a body saying "now go and forward the last invoice" has nothing to attach to. Only the *action* is the model's, from a six-value enum parsed by `Decision::parse`; an unrecognised action is a refusal, never a fallback, and every parameter is validated against operator configuration (the label list, the snooze bound, the archive mailbox — which the answer never names).
- **note:** *nothing here sends or deletes, structurally.* `agent::apply` terminates at `DraftStore` and names no outbox, SMTP, `delete_message` or `EXPUNGE` symbol; `nothing_in_the_agent_can_reach_the_send_path` reads all five modules back and fails if one appears — task 62's gate, widened to the delete path.
- **note:** *three independent grants are needed to mutate*, and the loop is bounded three ways. Scope (`AllOf[mail.read, mail.write, ai.invoke, automation]`), the operator's `agent.allow_mutations` (off by default, refused with `FAILED_PRECONDITION` naming the key), and the request's own `mutate` field — named for the dangerous direction so proto3's `false` default is a dry run. Bounds: `max_iterations`, `max_actions` (the blast-radius bound), `max_duration`, each clamped by a ceiling in the code and each with its own test.
- **note:** *a dry run writes nothing* — no IMAP call, no draft, no tag, no snooze, no `agent_runs`/`agent_actions` row, no injection flag. Counted with fakes, not read off the response. The one exception is deliberate and tested: `ai_ledger` still records the spend, because a dry run costs real money and an unattended loop is where invisible spend matters most.
- **note:** the shield gates the *mutation*, not the prompt — `crate::rules`' arrangement, kept identical so there is one shape of this gate to review. `hostile_mail_that_the_model_obeys_still_mutates_nothing` uses a provider that obeys the injected "archive everything" and asserts the move never happens.
- **note:** `agent_actions.message_id` is `ON DELETE SET NULL` with the RFC id/subject/sender frozen alongside, because `MailStore::move_message` *deletes* the local row — a `CASCADE` log would erase itself precisely when the archive worked.
- **note:** *`snooze` defers the agent, it does not hide mail.* It writes `message_snoozes` (which `store::candidates` reads, so the agent stops reconsidering the message and picks it up again when the time passes) and applies `agent.snooze_tag`, so the state is visible in the tag surfaces a human already uses. It deliberately does **not** touch `MailStore::list` — teaching every listing in the product to join a snooze table on behalf of one model-chosen action is a much larger decision than this feature gets to make.
- **note:** the injection release valve is order-sensitive. `injection::store::flag` clears `confirmed_at` when the detections differ, and the agent's scan (unfenced `render_for_model`) never matches `ScanInjection`'s (fenced, +22 bytes) — so the confirmation is *read before* anything is recorded, and a confirmed message is not re-recorded at all. Recording first nulled the consent a moment before honouring it, making the valve permanently unusable; `a_withheld_message_is_reconsidered_once_a_human_confirms_it` now confirms through the fenced rendering, which is what reproduces it.
- **note:** `KNOWN_TABLES` in `config` was missing `extract` (unreachable from the environment since task 75). Added, along with `agent`, plus `every_config_table_is_known`, which reconciles the list against the `Config` struct's own source so the next table fails by name.

## 70. AI periodic digest
- [x] status
- **depends-on:** 49, 43
- **parallel-safe:** yes
- **acceptance:**
  - Scheduled job clusters a window's mail by topic/sender and has Claude produce a ranked markdown briefing (needs-reply/FYI/waiting-on/auto-handled/skipped) with every line linked to source message-ids; `AnalyticsService.GenerateDigest`; `mail digest --since 7d`.
- **verify:** `cargo nextest run -p rmaild --test digest` (sectioned briefing, every line cites a message-id)

## 71. Response-time & SLA analytics
- [x] status
- **depends-on:** 9, 10
- **parallel-safe:** yes
- **acceptance:**
  - Pair sent replies to inbound via In-Reply-To/References; compute per-contact/per-mailbox p50/p90 response times + rolling trend; flag where the user is the bottleneck; `AnalyticsService.GetResponseTimes`; `mail stats response-time --by contact`.
- **verify:** `cargo nextest run -p rmail-core analytics::response_time` (pairing, percentile math, bottleneck flag)

## 72. Contact insights, subscriptions & NL analytics
- [x] status
- **depends-on:** 43, 9
- **parallel-safe:** yes
- **acceptance:**
  - Contact relationship insight (volume/direction/symmetry/cadence/topics → one-paragraph Claude briefing + decay report); newsletter/subscription detector (List-Unsubscribe + heuristics + Claude fallback, read-rate, unsubscribe-candidates + one-click); NL analytics (Claude → safe parameterized read-only SQL over whitelisted views + narrative).
  - `AnalyticsService.GetContactInsight/ListSubscriptions/AskAnalytics`.
- **verify:** `cargo nextest run -p rmail-core analytics::` (129 tests: subscription classification, the SQL whitelist guard rejecting writes, and the degenerate shapes — empty range, one message, never-replied, zero denominators) · `cargo nextest run -p rmaild --test analytics_service` (the three RPCs end to end) · `cargo nextest run -p rmail-cli analytics_cli` (nothing mail- or model-authored reaches the terminal unsanitized)
- **note:** `mail ask` was already feature 43's (`AiService/AskMailbox`, a question about message *contents*), so prd.md's NL-analytics verb is **`mail stats ask`** — the namespace `stats_cli` was created for. `mail contact` and `mail subs` are prd.md's own spelling.
- **note:** *nothing here unsubscribes anything.* A detected `List-Unsubscribe` is reported as a proposal — scheme-restricted to `https:`/`mailto:`, with the sender-chosen `mailto` query stripped — and no code path fetches it, follows a redirect or sends mail. `one_click` reports that the sender advertises RFC 8058; it enables nothing. A test asserts the module names no HTTP or SMTP type.
- **note:** `AskAnalytics` runs model-written SQL inside a SQLite **authorizer** (`analytics::sql`) that denies every action but a read reaching one of six `analytics_*` views (`V50`) and a call to a fixed function list. Enforcement is SQLite's own name resolution, not a regex — a CTE aliased to a view name, a subquery, `ATTACH`, `PRAGMA` and every write all fail at prepare time.

## 73. Structured invoice/receipt & data extraction
- [x] status
- **depends-on:** 22, 43
- **parallel-safe:** yes
- **acceptance:**
  - Detect invoice/receipt attachments; Claude with a strict schema pulls vendor/number/line-items/totals/currency/due/status into a queryable, CSV-exportable table; general `ExtractStructured` against a JSON schema (invoice/flight/meeting/etc.), validated and stored; `SearchService.SearchEntities`.
  - `mail invoices [--export csv]`, `mail extract <id> --schema invoice`.
- **verify:** `cargo nextest run -p rmail-core extract::invoice extract::structured` (79 tests: parsed-vs-inferred provenance through merge/SQLite/CSV, every bound, schema rejections) · `cargo nextest run -p rmail-core index::entities` (SearchEntities' query, `LIKE` wildcard escaping) · `cargo nextest run -p rmaild --test extract_service` (the four RPCs end-to-end: detection, re-extraction replaces, CSV formula guard, the INVALID_ARGUMENT/FAILED_PRECONDITION/NOT_FOUND paths)
- **note:** deterministic first — `index::entities` supplies every amount, date and reference (no second money parser), `extract::tables` supplies spreadsheet line items, and the model only fills what the document did not label. `merge` never lets an inferred figure overwrite a parsed one and records the disagreement instead. `ExtractStructured` lives on `ExtractService` rather than prd.md #4's `MailService` (that service did not exist when the PRD was written) and is the one extraction RPC gated `AllOf[mail.read, ai.invoke]`, because a caller-chosen schema has no deterministic route. CLI: `mail invoices [--export csv]`, `mail attach invoice <id> [part]`, `mail extract data <id> --schema invoice` (a subcommand rather than prd.md's bare `mail extract <id> --schema`, which clap cannot distinguish from `mail extract events`), `mail entities --search`.

## 74. Attachment semantic search & ask-your-attachment
- [x] status
- **depends-on:** 21, 22, 52
- **parallel-safe:** yes
- **acceptance:**
  - Extracted attachment text chunked+embedded and fused via RRF so "the termination-for-convenience clause" returns the exact attachment + page (`SearchService.SearchAttachments`); `AttachmentService.AskAttachment(stream)` answers a question scoped to one attachment/result-set with page/section citations, refusing unsupported answers.
- **verify:** `cargo nextest run -p rmaild --test attach_search --test ask_attachment` (page-cited answer, unsupported refusal)

## 75. Table, calendar/task & link extraction
- [x] status
- **depends-on:** 22, 43
- **parallel-safe:** yes
- **acceptance:**
  - Table extraction (native from spreadsheets, Claude vision for PDF/image tables) into typed rows with headers + source-cell provenance; calendar/task extraction (message + .ics → normalized events/tasks → .ics / pipe / task webhook, idempotent per message); URL/link extraction + Claude classification (unsubscribe/tracking/meeting/document/CTA) with relevance score + picker.
  - `AttachmentService.ExtractTables`, `ExtractService.ExtractEvents/ExtractTasks`, `LinkService.ExtractLinks`.
- **verify:** `cargo nextest run -p rmail-core extract::tables extract::events extract::links` (108 tests) · `cargo nextest run -p rmaild --test extract_service` (the RPCs end-to-end: idempotent delivery, the spoofed-link flag, per-cell provenance)
- **note:** PDF/image tables read the document's *extracted text* (page-marked) and an image's OCR output, not pixels — this crate has no PDF renderer and `ai::provider` carries no image content block, the same constraint `attach::ocr` documents. Such tables arrive with `origin = MODEL` and `inferred = true`.

## 76. Budget enforcer
- [x] status
- **depends-on:** 45, 46
- **parallel-safe:** yes
- **acceptance:**
  - Per-account + global daily/monthly token & dollar caps checked before dispatch; soft cap auto-downgrades the model (opus→sonnet→haiku), hard cap blocks; bulk jobs get a separate sub-budget; `AiPolicyService.SetBudget/GetSpend`; `mail ai budget set/status`.
- **verify:** `cargo nextest run -p rmail-core ai::budget` · `cargo nextest run -p rmaild --test ai_policy_service` (soft-cap downgrade, hard-cap block, bulk sub-budget)

## 77. Prompt-injection shield
- [x] status
- **depends-on:** 43, 47
- **parallel-safe:** yes
- **acceptance:**
  - Every body wrapped in untrusted-content delimiters and scanned for injection patterns (hidden text, zero-width chars, "ignore previous instructions"); detected messages flagged and any AI action on them requires confirmation, logged; `AiSafetyService.ScanInjection`; `mail ai scan-injection <id>`.
- **verify:** `cargo nextest run -p rmail-core ai::injection` · `cargo nextest run -p rmaild --test ai_safety_service` (pattern/zero-width detection, action-gating on flagged mail)

## 78. Local-only model path
- [x] status
- **depends-on:** 20, 43
- **parallel-safe:** yes
- **acceptance:**
  - Fully on-device inference route (candle/llama.cpp generation + local embeddings) exposing the same summarize/embed/draft verbs; forced by policy for local-only mail; outputs labeled locally-generated with zero egress; `mail ai provider set <account> local`.
- **verify:** `cargo nextest run -p rmail-core ai::local` (no outbound network under local provider, same verb surface; includes the workspace-wide source gate that fails by name on a new network client) · `cargo nextest run -p rmail-core ai::queue::tests::local_only_mail_is_served_on_device_rather_than_dropped ai::queue::tests::an_account_override_routes_otherwise_allowed_mail_on_device` (policy and the per-account override actually route the dispatch path)

## 79. OAuth2 broker (Gmail/Outlook)
- [x] status
- **depends-on:** 7, 8
- **parallel-safe:** yes
- **acceptance:**
  - Loopback-redirect OAuth2 + PKCE for Google & Microsoft; refresh tokens in Keychain; XOAUTH2 SASL for IMAP/SMTP; refresh-before-expiry; re-consent on revocation; `AccountService.BeginOAuth/CompleteOAuth/RefreshToken`; `mail account login --oauth <provider>`.
- **verify:** `cargo nextest run -p rmail-core oauth::` (PKCE flow against a mock authz server, XOAUTH2 string, refresh)

## 80. Unified inbox + AI account autoconfig
- [x] status
- **depends-on:** 8, 39, 43
- **parallel-safe:** yes
- **acceptance:**
  - Synthetic unified mailbox merging every account's Inbox into one time-ordered, Message-ID-deduplicated view with actions routed back to the correct account/folder (`MailService.ListUnified`, `mail list --all`).
  - Autoconfig probes ISPDB/SRV/autodiscover and, on a miss, hands domain+MX+probe responses to Claude to infer IMAP/SMTP settings, validates by login, writes a ready TOML block (`mail account add <email>`).
- **verify:** `cargo nextest run -p rmaild --test unified_inbox` · `cargo nextest run -p rmail-core autoconfig::` (dedup/order, probe→settings, login validation)

## 81. Priority notification engine
- [x] status
- **depends-on:** 48, 14
- **parallel-safe:** yes
- **acceptance:**
  - On each new-mail event Claude Haiku scores an importance tier + one-line reason; a macOS notification fires only at/above a per-account threshold so newsletters never ping; `NotificationService.ScoreMessage/StreamAlerts`; `mail notify watch`.
- **verify:** `cargo nextest run -p rmail-core notify::` (threshold gating, below-threshold suppressed) · `cargo nextest run --test notification_service` (RPC surface + `Status` paths)

## 82. Multi-format export
- [x] status
- **depends-on:** 9, 39
- **parallel-safe:** yes
- **acceptance:**
  - Export any query or thread to mbox / Maildir / .eml / JSON, streaming from SQLite and preserving raw RFC822; `--with-ai` batch-attaches Claude summaries + tags to the JSON; `ExportService.Export`; `mail export '<query>' --format mbox -o out.mbox`.
- **verify:** `cargo nextest run -p rmail-core export::` (each format round-trips, raw RFC822 preserved, --with-ai)

## 83. TUI shell (folders / list / preview)
- [x] status
- **depends-on:** 39, 33
- **parallel-safe:** no
- **acceptance:**
  - `ratatui`/`crossterm` TUI attaching to `rmaild` as a gRPC client (<200 ms startup); folders / message-list / preview layout; message viewer (plain/multipart/quoted-printable/base64/encoded headers, "open HTML in browser"); basic navigation `j/k gg G Enter q ?`; core actions archive/delete/mark/copy/move/reply/forward.
  - UI never blocks on sync/AI (reads local state via gRPC streams).
- **verify:** `cargo nextest run -p rmail-cli tui::model` (headless model/update tests; render smoke test)

## 84. TUI modal vim keymap engine
- [x] status
- **depends-on:** 83
- **parallel-safe:** no
- **acceptance:**
  - Layered keymap engine (normal/insert/visual, chord sequences), fully rebindable and hot-reloadable via `keys.toml`, mapping to named action ids shared by palette/gRPC/MCP; `ConfigService.GetKeymap/SetBinding`; `mail keys set <chord> <action>`.
- **verify:** `cargo nextest run -p rmail-core keymap::` · `cargo nextest run -p rmail-cli keys_cli::` · `cargo nextest run -p rmaild config_service::` (chord resolution, rebind, hot-reload, action-id shared registry)

## 85. TUI overlays (search / finder / AI panel / ask pane / palette / outbox)
- [x] status
- **depends-on:** 83, 84, 33, 59, 52, 61
- **parallel-safe:** no
- **acceptance:**
  - `/` streaming ranked incremental search (debounced, keystroke-cancel, `~`/`=` prefixes, operator autocomplete, `x` why-panel); Ctrl-P fuzzy finder + NL command palette (`CommandService.ResolveIntent`); collapsible AI panel + streaming Ask pane with citations; Outbox pseudo-folder with undo-toast countdown; AI quick-action menu (`.`).
- **verify:** `cargo nextest run -p rmail-cli tui::overlays` (search stream render, palette resolve, ask-pane citations, outbox undo)

## 86. Supply-chain & release gates
- [x] status
- **depends-on:** 1
- **parallel-safe:** yes
- **acceptance:**
  - `cargo deny check` and `cargo audit` wired as the final CI gate; `buf breaking` runs against the committed baseline on proto changes; criterion perf benchmarks assert the key budgets (first search hit, full ranked search, fuzzy first batch); macOS release packaging script for `rmaild`+`mail`.
  - Deny/audit/breaking failures fail the build.

---

# PART V — TUI REIMAGINING (Neovim-style commands, WhichKey, settings, manual)

Architecture proposal in full at the time these tasks were cut: see the session that produced them. Ground rules (do not weaken these; several tasks below exist specifically to extend them):

- **One shared vocabulary.** The `:` command grammar *is* the existing `Action` id namespace — `.` and ` ` are the same separator (`message.archive` ≡ `:message archive`). No parallel command registry.
- **`update` stays pure, synchronous, clockless.** WhichKey is a render-time function of existing state, not a new timer — see task 91's proof.
- **`Model::mode()` stays derived, never stored.** New screens are new derivation arms.
- **Modes are layers; overlay modes stop at `Global`.** A new `Settings` mode restates list bindings rather than inheriting `Normal`, exactly as `Menu`/`Pick` already do.
- **Help stays generated from data, never hand-maintained.** WhichKey, the command index, and the manual's capability footers are all derived from the live `Keymap` and `parity::Command` registry; only manual *prose* is authored.
- **Feature parity stays enforced.** Every new bindable `Action` must land in a `parity` capability row's `actions()` or in `LOCAL_ACTIONS` in the same commit, or `every_tui_action_is_a_capability_or_declared_local` fails by name.

Known backend gaps this phase deliberately does **not** paper over (Settings shows these as blocked/config-file-only rather than pretending an RPC exists): free-text NL fallback on the command line (no `CommandService.ResolveIntent`), `:ai redact preview`/set-redaction-level (no RPC), `:ai policy explain` (library-only, not exposed), general bulk preview/apply/undo beyond tags (no `BulkService`), Prompt Library, Conversation Memory, Mute/Kill-file.

## 87. TUI semantic theme module
- [x] status
- **depends-on:** 85
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/theme.rs`: a `Theme` struct of semantic tokens (surfaces, text, semantics, mail, keys, finder) and four built-ins (`dark`, `light`, `mono`, `high-contrast`); every `Style::default().fg(Color::…)` literal in `view.rs` replaced with a token lookup (`overlays.rs` had none).
  - `dark` is behavior-preserving: render tests assert byte-identical styling to today for the list, viewer, status bar, and each existing overlay. A lint test asserts no `Color::` literal remains outside `theme.rs`. `mono` carries every state by glyph or modifier alone (no color-only meaning), matching the existing unread/flagged/attachment glyph convention.
  - `Model.theme: Theme` (default `dark`), selectable via `mail tui --theme <name>`/`$RMAIL_THEME` (`ThemeName::from_id`, unknown name falls back to `dark` with a status-line notice, never a startup failure).
- **verify:** `cargo nextest run -p rmail-cli tui::theme tui::view` (frame parity under `dark`, no stray `Color::` literals, `mono` glyph/modifier coverage)
- **note:** daemon-activity and AI-spend token groups are deliberately not in this struct yet — nothing renders a daemon indicator or a spend meter until tasks 92/96, and a field nothing reads is exactly the half-finished state this project's non-negotiables refuse. Those tasks extend `Theme` when they land, the same way `keymap::Action` grows one variant at a time rather than pre-declaring a future task's vocabulary.
- **note:** `List::highlight_style` legitimately overrides a row's own span styling for whichever row the cursor is on, and `Style::patch` unions modifiers across the item/row/span layers rather than replacing them (an unread row's `Modifier::BOLD` reaches its own marker glyphs). Neither is a defect this task introduced; `chars_matching` (the render-test helper added here) checks "is this token realized" — unset `fg`/`bg` unconstrained, modifiers via `contains` not `==` — rather than exact `Style` equality, and assertions read a non-cursor row wherever a row's own styling is what is being verified.
- **closed at merge:** review (independently re-deriving `dark`'s mapping from the pre-refactor `view.rs`) confirmed the extraction itself byte-identical, then found four real defects, all fixed before this task counted as done: `Theme::high_contrast()` painted `muted` text white on a `sel_row` background also white (new code, no historical excuse); two render tests (`dark_theme_palette_chords_…`, `dark_theme_pick_confirm_and_input_overlays_border_…`) passed vacuously — the messages pane's own ambient `border_focus` border, or an unrelated `Color::Yellow` glyph elsewhere on screen, satisfied the assertion regardless of whether the overlay under test drew anything at all; and the `--theme`/`$RMAIL_THEME` fallback notice was overwritten by `Msg::Boot`'s "loading accounts…" within the same frame it appeared in, making the acceptance's "status-line notice" promise undeliverable in practice (`Msg::Boot`'s handler now skips that write when the model is already showing an error). `light`'s doc comment also claimed values the code didn't have; both were reconciled and are now pinned by `assert_eq!` against independently-built literals (`theme::tests::dark_is_the_historical_styling`, `light_matches_what_its_doc_comment_claims`) rather than a field-by-field spot check that had gaps a stray added modifier could pass through.

## 88. Command registry and parser (`rmail-core`)
- [x] status
- **depends-on:** 84
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail_core::command`: a verb registry with per-verb positionals, long-only flags, an optional `parity::Command` capability, an optional `Action`, and pure `parse`/`complete`/`describe` functions covering ranges (`'<,'>`, `%`, `N`), a trailing bang, quoted arguments, and dot-or-space verb paths. Every existing `Action::id()` resolves as a verb with zero registry entries needed.
  - Drift tests: every verb carrying a capability spells its path exactly as `parity::Command::cli()` does (modulo one declared `cli_alias` for `tag-rules`); no two verbs claim the identical path, and every real verb is reachable by parsing its own canonical spelling; every capability with a declared TUI verb is reachable from the registry. Errors name the offending token, in `KeymapError`'s idiom.
- **verify:** `cargo nextest run -p rmail-core command::` (parse/complete round-trip, range/bang/quoting, cli-path drift test, duplicate-path and self-reachability checks)

- **note:** the original acceptance text banned any verb path being a proper prefix of another, reasoning by analogy to `Keymap::bind`'s `shadow_conflict` for chords. Checked against the real registry, that analogy is wrong: `Keymap` commits to the *shortest* complete chord match as keys arrive and can never back out, but `parse_verb` deliberately tries the *longest* matching prefix of the typed words first — so a short verb (`search`, `outbox`) and a longer one sharing its first segment (`search.explain`, `outbox.cancel`) are both real, both intentional, and both always reachable by typing their own exact path; neither is dead code. The acceptance criterion and its drift test were corrected to prove reachability directly (parse every real verb's own canonical spelling, assert it resolves to itself) plus the one collision shape that is always a bug regardless of matching order: two verbs claiming the identical path.

## 89. Command line overlay and dispatch
- [x] status
- **depends-on:** 87, 88
- **parallel-safe:** no
- **acceptance:**
  - `Action::CommandOpen` (id `"command"`) bound to `:` in Normal/Viewer/Visual/Menu; `Overlay::Command(Box<CommandPane>)` deriving `Mode::Prompt`; `run_command` dispatches a parsed `Invocation`, delegating straight to the existing `run_action` whenever the verb carries an `Action` and no arguments — the 39 existing behaviours keep exactly one implementation.
  - `Overlay::Palette`/`PalettePane` deleted; `Action::PaletteOpen` re-points at the same command overlay (kept as a documented alias, since renaming an action id breaks a user's `keys.toml`); `palette_matches` generalized into the command completer, same 4-tier ranking.
  - Parse errors render in red inside the command line without closing the overlay. Opening `:` while `Model::visual.is_some()` pre-fills `'<,'>`. History is an in-memory ring plus a `0600`, 500-entry file written via `keymap::file::write_atomic`, prefix-filtered on `<up>`/`<down>`, never recording `token`/`account login` lines or any line with a `--*secret*`/`--*password*` flag.
- **verify:** `cargo nextest run -p rmail-cli tui::command` (dispatch delegates to `run_action`, palette alias still resolves, visual-range prefill, history redaction, parse-error-keeps-overlay-open)
- **note (verify line corrected):** there is no `tui::command` module — the tests live where the code does, in `tui::model::tests` (dispatch, ranges, bang, history), `tui::overlays::tests` (the ranked matches), `tui::history::tests` (the ring, the redaction rule, the file) and `tui::view::tests` (the line, the fallback marker, the red complaint). `scripts/docker-test.sh tui::` runs all four; a bare `tui::command` filter selects zero tests, which is the trap the header of this file is about.
- **note (up/down are the history, so the ranked list has no cursor):** the acceptance puts history on `<up>`/`<down>`, which are the keys task 85's palette moved its selection with — so the two cannot both be had. Vim's `:` wins: `<up>`/`<down>` walk the history prefix-filtered, `<tab>` completes through `command::complete`, and the ranked list is a preview that draws *no* selected row. `<enter>` runs the typed line, falling back to the best-ranked match only on `UnknownVerb` — which is the one failure meaning "you have not finished naming it yet", and is exactly what preserves the palette's own gesture (type a fuzzy name, press Enter). A malformed range or an unterminated quote is a line whose *shape* is wrong, and running something else because the verb inside it ranked first would be a keystroke doing what nobody asked. An empty line is not a fallback either: the list is then every verb in path order, and running whichever sorts first is not what a bare `<enter>` means. A `>` marker on row 0 says when the fallback is live, which is also what let `dark_theme_command_chords_match_the_historical_color` assert about row 0 at all — `List::highlight_style` overrides a row's own span styling wholesale, so under the palette that assertion could only ever be made about a row the cursor was *not* on.
- **note (ranges: one honoured, two refused by name):** `'<,'>` needs no code — every bulk-capable action already reads `Model::selection()`, so a `:` line carrying it does exactly what the key does with the same selection up, and delegation is the whole implementation. `%` and a leading count have no model support at all: nothing in this screen can address "every row listed" or "N rows down". They are refused with a message naming what is missing rather than half-honoured, because acting on the row under the cursor when `%` was typed is a range that looks obeyed and is not. `!` is applied once, after dispatch, to whatever `Confirm` the action opened — one implementation, so a bang cannot mean different things to different verbs.
- **note (`:manual <page>` was attempted and reverted):** the obvious way to make `open_manual_at`'s page-name seam reachable was to declare `manual` with an optional `page` positional. `command::tests`' `no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one` refused it, correctly: a verb taking positionals must not be a strict prefix of another, or the one word that collides — `grep` — silently means `manual grep` rather than being that argument. Declaring it anyway to serve a convenience would have made a guard advisory, and accepting the argument *without* declaring it (which `parse` would have allowed, since it collects trailing words whatever a verb declares) is the "quietly accepts an argument it never mentions" that `command::explicit`'s own docs call out. So `:manual archive` is refused by name, `open_manual_at` remains the direct seam, and task 102's `K`-on-a-row is still its first production caller. `:helpgrep <pattern>` and `:manual grep <pattern>` do reach `open_manual_grep_for` — that verb declares its positional — and the pattern is the *joined* positionals, because an unquoted multi-word pattern is what somebody types and searching only its first word would be a silent truncation.
- **note (history):** an in-memory ring of 500 in `Model::history`, plus a newline-delimited file beside `keys.toml` (`$RMAIL_COMMAND_HISTORY`, else next to the master config). Written through a `Cmd::SaveHistory` rather than from `update`, which stays pure, synchronous and clockless — the executor does it on `spawn_blocking`, and a failed write is logged and dropped rather than taking the command line down. The whole list travels rather than the new line, so the writer holds no state and a dropped write is corrected by the next one. `0600` is achieved by pre-creating the file private and letting `write_atomic` carry the destination's mode onto its temp file, so no rewrite ever exposes it at the umask's default. Redaction reads the *text*, not the resolved verb — a leading `token`, `account login`, or any `--…secret…`/`--…password…` flag — because the registry grows and a line is dangerous the moment it is typed; the range and bang are stripped first, or `'<,'>token …` would be a documented bypass. A line is recorded once it *parses*, including one then refused for its range, because that is precisely the line somebody wants `<up>` to bring back and fix.
- **note (`:` in `Menu` had to mean something):** the acceptance binds `:` in `Menu` as well as `Normal`, and `open_command`'s first draft refused whenever any overlay was up — which made the binding dead in the layer it was deliberately added to. It now *replaces* a list overlay, cancelling whatever that overlay was streaming, and still refuses over a picker or a confirmation, which own the keyboard because they asked a question. The third reading — open over the menu and restore it on Esc — is an overlay stack this model does not have, and would restore a search pane holding results whose stream had been cancelled.
- **closed at merge:** review (re-deriving each clause against the code) found two behavioural defects and four tests that could not fail. The first defect is the one that mattered: `open_command` and `unsupported_range` asked `model.visual.is_some()` rather than `Model::is_selecting()`, and the anchor deliberately outlives leaving the list — so a `'<,'>` typed in the viewer passed the range check, then `targets()` returned the viewer's single message, and the range was silently a no-op. That is exactly the "looked honoured and was not" the `%` refusal exists to avoid, and `is_selecting`'s own doc comment names it as the mistake that once let `a` archive the viewer's message while `r` refused. Both sites now ask the same question every other reader asks. A range is also refused for a verb that reaches no capability at all — `:'<,'>help` — derived from the parity table rather than a second list. The second defect: the Enter fallback rebuilt `range + verb + bang` and dropped flags, so `:message archive --force` was refused by the parser while `:arch --force` archived without a word; a line carrying a flag is now refused rather than guessed at. Review also caught two `<tab>` bugs — `:search --x` completed to `search search`, and the completer counted the prefilled `'<,'>` as typed text, which made Tab dead in the state the command line is documented to open in — a secret already in the history file being loaded, offered on `<up>` and written back out (`History::new` now filters, so the rule is self-healing), a `write_atomic` that created its temp with `fs::write` and chmod-ed after (so the full content existed world-readable for a window on *every* rewrite, for `keys.toml` too — the temp is now created `0600`), a startup `history::read` doing filesystem work on a runtime thread with the terminal already in raw mode and no bound on `open(2)` against a FIFO, and a complaint rendered without `safe_line` while the line above it was sanitized. The four vacuous tests: "refused over a modal" pressed `:`, which is not bound in `Confirm`, so it was green for any body of `open_command`; the secret-line test typed a line that never parses, so it passed with the redaction deleted; the path test asserted only that a path has a file name; and both `<tab>` tests took the single-candidate branch, leaving `longest_common_prefix` with no coverage at all. `Mode::Viewer` — one of the four modes the acceptance names — had no test either.
- **note (the manual moved with it):** task 104's pages said in as many words that this build had no command line. `manual`, `tour`, `bulk`, `triage-by-selection` and `practice-export` now describe the one it has — including which ranges work and which are refused — and `command` is declared in `tour`'s `documents`, which is what `every_action_and_verb_has_exactly_one_documenting_page` demands of any new action.

## 90. Generic Report overlay
- [x] status
- **depends-on:** 89
- **parallel-safe:** no
- **acceptance:**
  - `Overlay::Report(Box<ReportPane>)` in `Mode::Menu`: fixed-width columns, append-or-replace rows, the existing `generation` supersession discipline (matching `SearchPane`/`FinderPane`), `Cmd::CancelStream` fired on Esc for an in-flight stream, `r` re-runs the pane's own stored `Invocation`, and each row carries `on_enter: Option<Invocation>` gated by `parity::Command::effect()` so a mutating row asks for confirmation first.
  - One overlay type serves line/table/stream results — no per-domain report screens.
- **verify:** `cargo nextest run -p rmail-cli tui::report` (streamed report superseded mid-flight drops stale frames and cancels the stream, mutating row confirms, `r` re-issues the same invocation)
- **note:** task 103 landed first and draws `manual::grep`'s cross-page hits as a generated manual page (`Location::Grep`) rather than as a Report, because no Report existed to draw them in. That is the one place "no per-domain report screens" is currently violated. `manual::grep` is already a pure `(pattern, &Keymap) -> Vec<GrepHit>`, so re-pointing it is a `ReportPane` built from those rows plus deleting `Location::Grep` — decide here whether a hit list that wants the manual's own back stack is better served by staying where it is.
- **note (verify line corrected):** the tests are in `rmail-cli/src/tui/report/tests.rs`, so `tui::report` genuinely selects them — the pane, the merge rules, the gate, and the end-to-end key behaviour driven through `tui::model::update`, all in one module for the reason `tui::overlays::tests` gives (a bare filter matches a test's *name*, so a suite split across two module paths is half-selected by the command that claims to run it). `scripts/docker-test.sh tui::report` runs 48. The daemon-level half is `rmail-cli::bin/mail auth_status_reports_the_daemons_gate_and_this_clients_credential`, in `tui::grpc::tests` where the in-process `rmaild` harness lives.
- **decided (`manual::grep` stays a manual page):** not re-pointed, and the reason is the acceptance's own `on_enter: Option<Invocation>`. A grep hit's Enter has to open the page it was found on, and there is no invocation that says so: task 89 tried `manual <page>` and `no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one` refused it (a verb taking positionals must not be a strict prefix of another, and `manual grep` is the collision). A row whose action is not expressible as an `Invocation` is not a report row, so a Report of grep hits would be a hit list with a dead Enter — losing the manual's back stack, its in-page highlight and `manual.next-match` to satisfy a slogan. `Location::Grep` is a *manual* page whose rows navigate the manual's own document space, which is a different thing from a report of daemon state; "no per-domain report screens" is read as the rule it is — one Report for every row-shaped *answer*, which is what tasks 94–100 produce.
- **note (the first caller, and why it is `:auth`):** a Report nothing can open is the "shipped inert" failure this project keeps finding, and every daemon verb in `tasks.md` belongs to 94–100. `ClientAuthService` is the one capability family no remaining task claims, so task 90 declares `auth status` (Read) and `auth clear` (Mutate) in `command::explicit` — the first entries there that reach a `Capability` with **no** `Action`, which is the shape `every_declared_verb_spells_its_capability_like_the_cli` was written for and had never had a registry row to check. Both spell their capability exactly as `mail auth status`/`mail auth clear` do, so neither needs a `cli_alias`. `:auth status` renders the report; the "password configured" row offers `:auth clear` on Enter, which is what exercises the effect gate through production code rather than through a fixture. `:auth clear` also forgets the session cached for the socket and refuses under `--addr`, both exactly as `auth_cli::clear` does — `client.rs`'s token resolution was extracted into one `credential()` so the report's "this client presents …" row cannot claim a precedence the connector does not follow.
- **note (append *and* replace both have production callers):** `ReportFill` exists because the two streamed panes disagree for good reasons — search appends (each hit sent once, in rank order), the finder replaces (a bounded top-K heap can evict an entry it already sent). `:auth status` answers from two sources, so it uses both: the daemon's two settings are the complete state of the gate and *replace*, and the credential row comes from argv and the keychain and *appends* rather than erasing what the daemon said. It is sent even when the RPC failed, and before the failure frame, because "the daemon did not answer and I am presenting nothing" is the most useful thing that screen can say. Row-keyed updates were considered for streamed progress and rejected: a progress table is a handful of rows re-sent, and a keyed merge would be a second rule to keep in step with this one for no behaviour the snapshot does not already have.
- **note (a row action keeps the report, and marks it stale):** `Overlay::Confirm`'s `message_ids` became `Confirmed::{Delete, Invoke}`, and `Invoke` carries the report the question was asked over. Without that, this model's single overlay slot means a mutating row destroys the screen it acted on — which is not a mechanism task 95's accept/reject-inline suggestion list can use, since every acceptance would close the list. Either answer therefore leaves the reader on the report: declining puts it back untouched, running puts it back marked `stale`. Stale rather than re-read, because the mutation is still in flight when the row's command returns — a refresh issued there races it and can redraw the state from *before* the change, which is a wrong answer with nothing saying so. `r` is what re-reads, and it is what clears the marking.
- **note (`r` issues rather than cancels):** `Cmd::CancelStream` is fired on Esc, as the acceptance says, and deliberately *not* on `r`. `restart_search` already records the rule — "a superseding request is what cancels these, and issuing none supersedes nothing" — and `tui::grpc`'s `reporting` slot aborts the task a new request replaces. An explicit cancel on `r` was written first and removed: it would have been a second mechanism for supersession, free to drift from the one every other streamed pane uses.
- **closed at merge:** `complain` wrote only into the command line, so every refusal from a report row — a verb given an argument it does not take, a range it cannot honour — was a keystroke that silently did nothing; it now falls back to the status line, which is the only surface a row's dispatch has. `run_invocation`'s new arms were originally placed *before* the flag guard, so a flag on a reporting verb would have been dropped on the way past rather than refused. `report_cells` used `truncate_chars`, which answers `max + 1` characters when it cuts — so one elided cell pushed every column after it one place right, on that row only; `fitted` puts the ellipsis inside the width. The header row was built by prepending a glyph placeholder to the *cells* rather than indenting the line, which shifted every heading one column left and dropped the last one entirely. `Cmd::AuthClear` cleared the session cache inline, which is a synchronous Keychain call on a runtime thread — the one thing `client.rs` documents at length must go through `spawn_blocking`. Two tests could not fail: the two column-alignment assertions compared `str::find` byte offsets, and a row carrying a three-byte ellipsis is two bytes further along while sitting in the same screen column, so they failed for the wrong reason and would have passed a genuinely misaligned grid; and `a_verb_whose_action_reaches_a_mutating_capability_mutates_too` was vacuous, because an auto-derived verb's own `capability` field is filled in from its action — the second read in `mutates` is there for a hand-declared verb that names an action and forgets the capability, and only a constructed fixture can exercise it.

## 91. WhichKey band
- [x] status
- **depends-on:** 87, 89
- **parallel-safe:** no
- **acceptance:**
  - `Keymap::continuations(mode, prefix) -> Vec<Continuation>` and `Keymap::shadowed_across_layers() -> Vec<(Mode,Chord,Chord)>` added to `rmail-core`. A bottom band renders whenever `!pending.keys().is_empty()` (never on a count-only pending) **or** the command overlay is open (showing completion candidates instead). Group labels are derived from the longest common dot-prefix of member action ids — no hand-maintained group table. `<esc>`/`<c-c>` are pinned in every band; a continuation killed by cross-layer shadowing renders struck-through with a warning.
  - A proof test documents *why* zero delay is correct: for every mode and every bound prefix, `pending.keys()` is non-empty only after a `Keymap::lookup` on that prefix already returned `None` — so nothing pending could have fired on its own, and no timer is needed.
- **verify:** `cargo nextest run -p rmail-core keymap::continuations` · `cargo nextest run -p rmail-cli tui::whichkey` (band key-set equals extendable-key set for every mode/prefix, count-only pending renders no band, shadow warning renders)
- **note (both verify lines select what they claim):** the two functions live in a new `rmail-core/src/keymap/continuations.rs` with its own `tests`, and the band lives in a new `rmail-cli/src/tui/whichkey.rs` with its own — so `keymap::continuations` runs 17 and `tui::whichkey` runs 21, rather than the zero a filter naming a module that does not exist would have selected. `Continuation`/`Leads` are re-exported from `keymap`, so the acceptance's `Keymap::continuations(mode, prefix) -> Vec<Continuation>` reads exactly as written.
- **note (what a "killed continuation" turns out to be):** a continuation of a *pending* prefix is always reachable — a prefix is only pending because `resolve` already looked it up and found nothing, so nothing shorter along it can fire. What cross-layer shadowing actually kills is a binding *under* a continuation that completes: `Normal` has the three-key `zab`, `Visual` binds the two-key `za`, and in Visual the `a` runs Visual's binding while nothing ever waits for `b`. `Continuation::buried` therefore carries those chords **with their own actions** — `lookup` on a dead chord answers the *killer's* action, so this is the only place the dead binding's own meaning survives — and the band draws one struck-through entry per buried chord plus a warning row. `bind` cannot refuse this: it sees one layer, and `g` in the viewer is a legal edit whose consequence belongs to the chain rather than to that line.
- **note (the key-set test needed an independent oracle, twice):** the acceptance's headline claim is "band key-set equals extendable-key set for every mode/prefix", and the first draft compared `continuations` with itself, which cannot fail. It now checks against `extends()`, built from `resolve` plus `lookup` — and `resolve` alone is not enough, because a dead sequence retries its own tail: `g` then `j` *runs* `cursor.down` while `gj` extends nothing, so a `Run` only counts when `lookup` agrees the whole sequence is bound. The second draft then failed for a real reason: it built a model whose `mode()` was `Normal` and compared its band against the loop's mode, and `Mode::Help` has both `gg` and `g/` (`manual.grep`), so the two answers genuinely differ. The quantified claim now calls `chord_band(keymap, mode, prefix)` directly and `the_band_reads_the_mode_the_model_is_in` covers what `band` does with the model.
- **note (group labels, and what an unlabellable group says):** the longest common *dot-prefix* of the member ids, segments not characters — `manual.back` and `menu.accept` share the letter `m` and no segment, and answering `m` would name nothing. Members with no common leading segment get no label at all and the band shows their count instead: inventing a name for an arbitrary collection is exactly the hand-maintained group table the derivation replaces.
- **note (`shadowed_across_layers` reports the killer that fires):** one entry per dead binding, named against the *shortest* bound prefix, since `resolve` runs the first one it reaches — with `a`, `ab` and `abc` all bound, `abc` is reported once against `a` rather than twice. It also covers same-layer conflicts, which `bind` does not: `Keymap::defaults` installs through `insert`, so the built-in table is outside that check and `the_default_bindings_shadow_nothing_across_their_layers` is what covers it. Task 105 wires it as the startup lint and `:keys check`.
- **note (the band is data, and the pins are literal):** `whichkey::band(&Model) -> Option<Band>` is pure and `tui::view` maps it onto styles, the same split `manual::Ink` keeps — which is why the struck-through claim is testable without a terminal. `<esc>`/`<c-c>` are pinned in *every* band including the command line's, and they are named as chords rather than looked up by action because that is the direction the guarantee runs: `Chord::is_reserved` refuses to bind anything starting with either, so the band's promise is one the engine keeps. Their labels are still derived. The cap (`MAX_ENTRIES`) comes out of the middle rather than off the end, so a `keys.toml` with forty continuations under one prefix cannot push the way out off the band.
- **note (no delay, and the proof):** `a_pending_prefix_is_always_one_that_resolved_to_nothing` walks every prefix of every binding in every configurable mode, keeps only the ones `resolve` actually holds, and asserts `lookup` on each returned `None` — so nothing half-typed could have fired on its own and there is nothing for a timer to disambiguate. It also asserts the reverse direction (something extends every held prefix) and that it checked more than zero of them, which is what stops it from being green over an empty loop.

## 92. Status bar and daemon heartbeat
- [x] status
- **depends-on:** 87, 90
- **parallel-safe:** no
- **acceptance:**
  - Status bar restructured into fixed-width zones: mode (all ten modes render, not three), account/folder/unread-total, message, a daemon indicator zone, inflight, pending.
  - `Cmd::Heartbeat` fans out to `SyncService.Status`, `IndexService.Status`, `AiService.GetUsage`, and `AiPolicyService.GetSpend` every 5s **without incrementing `inflight`** (a heartbeat that blinked the busy marker forever would destroy the one signal it carries), superseded by `WatchEvents`/`WatchOutbox`/`StreamAlerts` where those already push. Each indicator carries a glyph as well as a color and names the `:` command that expands it into a Report.
- **verify:** `cargo nextest run -p rmail-cli tui::status` (all ten mode labels render, heartbeat never touches `inflight`, indicator glyph/color pairs for each daemon state, push events preempt polling)
- **note (the verify line selects what it claims):** the bar is a new `rmail-cli/src/tui/status.rs` with its own `tests`, so `tui::status` runs 29 — the zones, the ten labels, the glyph/tone table, the four proto→state mappings, and the heartbeat's effect on `inflight`. The daemon-level halves are `a_folder_listing_also_reports_the_sync_indicator` and `the_heartbeat_reports_every_subsystem_it_claims_to_ask_about`, in `tui::grpc::tests` where the in-process `rmaild` harness lives.
- **note (mode labels are derived, and two of them changed):** `-- {Mode::id().to_uppercase()} --`, not a ten-row table — so a mode a later task adds is labelled without an edit, and `Mode::CONFIGURABLE` plus `Global` is where "all ten" comes from (`mode()` never returns `Global`; it is labelled because the function is total over the enum). Two shipped labels therefore changed: `Mode::Menu` was `-- SELECT --` and is now `-- MENU --`, and `Mode::Prompt` was `-- INSERT --` and is now `-- PROMPT --`. The second is the point of the clause: they are different layers with different bindings, and the bar saying which is live is how somebody works out why `<tab>` did something unexpected.
- **note (the `:` command each indicator names is derived, and is dark until 94/96):** `Indicator::expands` asks `command::verb_at` and is `None` when the verb does not resolve. `:index status` is task 94's and `:ai budget status` is task 96's, so three of the four hints are dark in this build and light up with no edit here when those tasks declare their verbs. Naming a command that answers "unknown command" would have been the bar telling somebody to type something that does not exist — the shape of defect this project's reviews keep finding. `the_hint_is_the_verbs_own_canonical_spelling` exercises the derivation against `auth status`, which task 90 declared, so the mechanism is proven rather than merely written.
- **note (what "superseded by the push streams" could actually mean here):** `WatchEvents` does *not* refresh the sync indicator today, and the reason is worth writing down: `Msg::Changed` issues `Cmd::LoadMessages`, not `Cmd::LoadFolders`, so a push reloads the open folder's rows and leaves the folder list alone. What is implemented instead is the honest version of the same idea — `Cmd::LoadFolders`' RPC *is* `SyncService.Status`, so the executor reports the sync indicator from that one call, and any reload (boot, a folder switch) preempts the heartbeat's next tick rather than the two racing to say the same thing. `WatchOutbox` and `StreamAlerts` have no TUI subscriber at all yet (tasks 98 and 100), so there is nothing there to supersede. Making `Msg::Changed` also reload the folder list would close the remaining gap and keep the folder counts fresh, which they are not today — deliberately left, because that is task 83's behaviour and not this acceptance's.
- **note (unread is the loaded rows, and says so):** no RPC in the API reports a folder's unread total — `FolderStatus` carries `message_count` and nothing else — so the scope zone counts the rows this client has fetched. It is drawn as `personal/INBOX 1▾` and omitted entirely when nothing loaded is unread, rather than labelled as a folder total it is not. Closing this properly wants a field on `FolderStatus`, which is a proto change and therefore not this task's.
- **note (a pre-existing defect the tests found):** task 93's focus hint named `<tab>` literally and checked only screen and focus, so it appeared under the folder *picker* — where `<tab>` is bound to nothing, because `Mode::Pick`'s chain stops at `Global` — and it went on saying `<tab>` after a `keys.toml` rebound `focus.toggle`. The chord now comes from `Keymap::chords_for(model.mode(), Action::FocusToggle)`, and an empty list is the honest answer to both cases at once.
- **note (drop order, and the one zone that is not informative):** narrowing drops the daemon indicators, then the account/folder, each only while the message zone keeps `MIN_MESSAGE` columns — the message is what carries whatever just failed, so an indicator that squeezed it to nothing would have hidden the sentence explaining why that indicator went red. The focus hint is deliberately *not* subject to that floor: it is eligible only at a width where the folder pane is not drawn, and at that width a `Focus::Folders` state makes `j`/`k` move a cursor nobody can see — a fact about what the keyboard is doing, which is the class this bar never drops. A first draft had it dropped first and it was therefore never shown at any width, since the hint plus the floor did not fit under 62 columns.
- **note (the busy marker lost its words):** `2 in flight` became `⧗2`. A zone wide enough for the sentence is a zone taken permanently from the message beside it, and the glyph-plus-count matches the indicator zone's own discipline. Zero draws nothing at all, because a permanent `0` is a permanent claim that nothing is happening.
- **closed at merge:** the four `wire::*_health` mappings had no tests at all — the glyph/tone table did, which made "indicator glyph/color pairs for each daemon state" look covered while the mapping from proto to state was not; found by reverting `ai_health`'s `enabled` check and watching the suite stay green. They are tested now, including the trap `ai.proto` warns about in as many words: a daemon with `ai.enabled = false` never spawns the dispatch loop, so `paused` stays false, and reading that as "running" would send somebody to resume something no RPC can start.

## 93. Responsive layout and toast queue
- [x] status
- **depends-on:** 87
- **parallel-safe:** yes
- **acceptance:**
  - `render_panes` drops the preview column below ~100 terminal columns and the folder column below ~60 (2-pane, then 1-pane; `<tab>`/`h` still switch focus); pane widths and the AI-panel width become `:set` options; `Model::summary_pinned`'s pin state becomes visible in the AI panel header.
  - The single toast row becomes a capped queue (1 visible + `+N` badge) carrying undo countdowns (existing), completion notices, and priority alerts, without growing past its existing one-line reflow.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (breakpoint transitions at ~100/~60 columns, toast queue caps at 1+N, pin indicator visible)
- **as built:** `:set <option> <value>` is a new explicit registry `Verb` (both positionals `required: false` — a `required: true` pair fails `every_real_verb_is_reachable_by_typing_its_own_path`, which admits no exceptions, so "both are mandatory" is `set_option`'s job, not the grammar's); it gained `Verb.description` since it's the first verb with neither an action nor a capability for `describe()` to borrow a sentence from. `Model.toast: Option<UndoToast>` became `Model.toasts: VecDeque<Toast>` (cap 5); `Toast::Undo` is the only variant with a real producer today. `Toast::Completion`/`Toast::Priority` exist (`#[allow(dead_code)]`, exercised only by tests) as the queue's declared shape for what tasks 94/98 will push — not wired to a real source, and deliberately not given a self-expiry: `Msg::Tick` is driven solely by `Cmd::Countdown` from an armed undo window (`arm_toast`/`grpc.rs`'s countdown timer), not a free-running heartbeat, so a tick-based TTL for these would only fire while an unrelated undo toast happened to also be live. Until 94/98 thread a real clock or event to a push call site, this queue's lifecycle is "shown until evicted by the cap" — `shown_toast()` ranks Undo > newest Priority > newest overall, and `push_toast` evicts the oldest *non*-Undo entry so a flood of other toasts can never silently swallow a live undo offer. `render_panes` measures the area *after* the AI panel's own split (`render_main_with_panel` takes its share first), so the two breakpoints are against the panes' own width, not the raw terminal — `panes_width()` re-derives that same number for `render_status`, which now shows `focus: folders (<tab>)` in a reserved column (not appended after the unbounded `model.status`, which would truncate it off narrow terminals) whenever `Focus::Folders` is off-screen — `update` has no `Msg::Resize`/terminal width, so this makes the state legible rather than preventing it.

## 94. Daemon-observability commands
- [x] status
- **depends-on:** 90, 92
- **parallel-safe:** no
- **acceptance:**
  - `IndexService` (all 7 RPCs), `SyncService.SyncFolder/Pause/Resume/Status`, `AiService.GetUsage/SetPaused/RetryFailed/AnalyzeMessage`, and `FinderService.RebuildIndex/IndexStatus` wired behind `:index status|run|start|stop|reindex|rebuild|verify|gc|entities`, `:sync now|pause|resume|status`, `:ai status|pause|resume|retry|process|cost`, `:finder rebuild|status`.
  - Streaming verbs (`reindex`, `rebuild`) render live progress in a Report; `:index rebuild` refuses without confirmation unless suffixed `!`.
- **verify:** `cargo nextest run -p rmaild --test tui_index_commands` (or equivalent integration harness) covering each verb's happy path and the rebuild confirm/bang gate
- **note (the harness is the equivalent one, and it is in `rmail-cli`):** `rmaild/tests/` cannot reach `tui::grpc::GrpcExec` — the executor lives in the binary crate, which has no lib target — so the integration half is `tui::grpc::tests`, which already runs a real in-process `rmaild` over a real Unix socket. Three tests there cover every verb's happy path against the daemon: `every_daemon_report_verb_reaches_an_rpc_and_comes_back_as_rows` (the eight unary reports), `a_streamed_rebuild_reports_progress_and_completes` (the streaming path) and `the_daemon_control_verbs_answer_with_a_labelled_fact`. `scripts/docker-test.sh tui::commands` runs the 29 table-and-dispatch tests, which is where the verify line's other clauses live.
- **note (the shape: a table, not a dispatcher):** the twenty-one verbs are one pure function, `tui::commands::answer(invocation, target, generation) -> Option<Answer>`, returning the `Cmd` plus the Report's title and columns. `tui::model::run_daemon_command` is the only code that turns one into an overlay, a request or a refusal — so the confirmation gate, the generation stamp, the Report and the status line exist once however many verbs the table grows to, and tasks 95–100 add rows rather than dispatch. Task 90's `report_spec` and its `auth clear` special case were folded into it, so `:auth status`/`:auth clear` now go through the same path as everything else.
- **note (rows or a fact, and it is not "does it mutate"):** a verb answering with more than one number opens a Report; one answering with a single fact says so on the status line. `:sync now` mutates and answers with a row per folder, and reducing that to "synced" would throw away the one thing somebody ran it to see; `:ai resume` either resumed or did not. Facts are counted into `inflight` (somebody asked) and reports are not (the report *is* the progress).
- **note (the confirm gate is per verb, and `:index rebuild` is the only entry):** task 89 settled that a `:` line typed in full is already the deliberate act a confirmation asks for, so gating every mutating verb would make the question meaningless by asking it twenty times — `:index gc` deletes rows and does not ask. Rebuild asks because it drops every derived row and leaves search degraded for minutes, which is a judgement about that verb rather than about its effect class. It rides on the spec rather than in a second table, so a verb cannot be added without the author looking at the field, and `rebuild_is_the_only_verb_here_that_asks_when_typed` walks the registry to keep the list honest. `!` skips it, exactly as `mail index rebuild --yes` does. `Confirmed::Invoke.over` became an `Option` for this: a question asked of a typed line has no report behind it to put back.
- **note (`r` never re-asks):** the question was answered to open the report, and asking it on every `r` would make `r` the wrong key to press. `rerun_report` therefore re-asks the table with `bang: true` — which also means a re-run takes the same path a typed `!` does rather than a second one that could drift.
- **closed at merge — two defects only a daemon could find:** `:index rebuild` sent `RebuildRequest::default()`, and the daemon requires `confirm: true` — so the verb answered `FAILED_PRECONDITION` every single time and no table test could have noticed. And `:index entities` sent an empty `kind`, which `ListEntities` refuses with the list of kinds it knows; the verb now takes the kind as a positional. That positional is *optional* rather than required, because `command::tests::every_real_verb_is_reachable_by_typing_its_own_path` refuses a verb that cannot be typed as its own path — rightly, since the registry is also the generated command index and a row nobody can type documents nothing — so a bare `:index entities` is refused client-side with a message naming some kinds, exactly as `mail entities` refuses it.
- **note (a guard that was wrong for the new verbs):** `run_invocation` refused *any* positional with "takes no arguments", which was true while every verb was action-backed and became wrong the moment one declared an argument. It now compares against the verb's own declared count, read off the registry — so `:index entities email` is accepted, `:index entities email phone` is refused by number, and `:message archive now` still says what it said. `command::parse` collects trailing words whatever a verb declares (task 89's own note), so without this a verb would silently accept an argument it never mentioned.
- **note (three places the TUI had to match the CLI rather than invent):** `:index gc` sends `purge_search_caches: false`, which is `mail index gc`'s default — the caches invalidate structurally and each discarded query plan is a paid model call. `:sync now` sends `SyncMode::Auto` and no mailbox, which is `mail sync` without `--full`. `:index run` is drain mode and `:index reindex` is selection mode, which is how `AiGetUsage`-style multi-verb capabilities are spelled in `cli()` — collapsing them into a flag would have been the surface where the spelling diverged.
- **note (what the streaming reports draw):** `Reindex`/`Rebuild` frames are `IndexProgress` snapshots of running totals, so each frame *replaces* — appending would draw one row per tick, a scrolling log of the same five numbers. A stream that ends without the terminal `done` frame is reported as a failure rather than as a finished pass, because both RPCs promise one. `:ai process` deliberately does not draw the model's prose: the analysis is what the AI panel shows once cached, and two surfaces for one answer is one too many — so it counts tokens, which is the only visible sign a model call is alive.
- **note (task 92's indicator hints lit up with no edit):** three of the four now name a verb (`:sync status`, `:index status`, `:ai status`) because `Indicator::expands` asks the registry. `:ai budget status` is task 96's and is still dark, which is what keeps that `Option` load-bearing.

## 95. Tag and rule commands
- [x] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - All nine `TagService` RPCs and all six `RuleService` RPCs wired behind `:tag add|rm|list|new|bulk|suggest|rules` and `:rule list|new|add|run|backtest|correct`, including ranged `:'<,'>tag add <tag>` and a streaming `:tag suggest` Report whose rows accept/reject inline.
  - This is `RuleService`'s first human surface anywhere — its command spellings are what a future `mail rule` CLI verb must adopt (recorded in the manual per task 104).
- **verify:** `cargo nextest run -p rmail-cli tui::commands::tag tui::commands::rule` (ranged tag ops, suggest accept/reject, rule dry-run and backtest rendering)
- **note (the verify line's module paths now exist):** task 94's single `tui/commands.rs` became `commands.rs` plus `commands/{daemon,tag,rule}.rs`, each domain a table of its own with its own `tests` — so `tui::commands::tag` and `tui::commands::rule` select 68 between them rather than the zero a filter naming a module that does not exist would have. `commands.rs` keeps the shared types, the argument helpers, and the dispatcher that asks each domain in turn; `command::tests::no_two_real_verbs_share_the_same_path` is what makes "first answer wins" a statement about evaluation order rather than a precedence rule anybody has to remember.
- **note (sixteen verbs, and the two the acceptance's list does not name):** `:tag rules set` is nested under `:tag rules`, which `command::explicit`'s own `cli_alias` docs anticipated by name. `:tag accept <id>` and `:tag reject <id>` are the other two, and they are not optional: "rows accept/reject inline" needs a row action, a row action *is* an `Invocation` (task 90), so the two gestures need two verbs. Both spell their capability as `mail accept-tags`/`reject-tags` do.
- **note (`report.reject`, and why a row needed a second gesture):** a `ReportRow` had one action, so a suggestion list could accept inline and only reject by typing — which makes the safe answer the awkward one on a screen whose common reply is "not that one". `ReportRow::on_reject` plus `Action::ReportReject` on `n` in `Mode::Menu` is the whole addition. Named for what it does rather than as a generic `on_alt`: a name that says nothing about what the key does is worse than a specific one a later task has to think about before adding a third.
- **note (suggestion rows carry a bang, deliberately):** both `tag accept` and `tag reject` mutate, so task 90's gate would open a modal for *every* answer on the suggestion list — and hardest for `reject`, which is the undoing direction. The rows therefore build bang'd invocations: the gesture is the consent, and the border says so (`Enter accepts · n rejects`). The gate still fires for any other mutating row, which `tui::report::tests` covers.
- **note (`:rule add` takes no argument, and the three options that were weighed):** `CreateRule` takes a TOML document and a one-line grammar cannot carry one. A file path reads well and is what a CLI form will be, but it puts a user-supplied path and a blocking read into the TUI's executor for one verb. An `Input` overlay has the same problem as a positional. What shipped is the flow somebody actually wants: `:rule new <what it should do>` synthesizes from words and shows the TOML plus a dry run over real mail, and `:rule add` stores that draft — so the dangerous half is only reachable after its dry run has been on screen. A hand-authored file still goes in through `mail api call RuleService.CreateRule`. The draft is `Model::rule_draft` rather than a field on the `ReportPane`, because it outlives the report (read, close, think, add) and a generic overlay growing one field per verb is how it stops being generic.
- **closed at merge — a documented behaviour the grammar never had:** `Flag::takes_value`'s doc has said "`--name value` (true)" since task 88, and `tokenize` only ever split on `=`; the test *named* `a_flag_with_an_equals_value_parses_the_same_as_a_space_separated_one` built only the `=` form. Nothing noticed because no verb in the registry declared a flag until these did. `parse` now pairs a value-taking flag with the word after it, which cannot live in `tokenize` (only the `Verb` knows which flags take values, and a tokenizer guessing would swallow the positional after a switch). Two limits are now asserted rather than implied: `tag rules --mode set` resolves as the longer verb, because longest-prefix is the rule everywhere else; and a *space-separated* value ahead of the verb path is indistinguishable from a path segment and fails as an unknown verb, while the `=` form works anywhere. vim's `:` has the same shape.
- **note (a second guard that was wrong for the new verbs):** `Positional` gained `rest`, and `run_invocation`'s count check honours it — `:rule new archive newsletters from marketing` is one instruction, and task 94's "more arguments than declared" guard refused it. Declared on the positional rather than as a list of verb names in the dispatcher, and `PATTERN` is marked with it too: `:helpgrep two words` has always joined them, so the declaration now says what the verb does.
- **note (three defaults copied rather than chosen):** `:tag rules set`'s mode defaults to `suggest` and its floor to `0.9`, which is what `mail tag-rules set` defaults to — and the safe half of the pair, since without a rule at `auto` nothing is applied on anybody's behalf. Its disable switch is `--disabled` and not `--off`, because two surfaces over one capability disagreeing about a flag name is the drift `parity` exists to prevent. A `--min-conf` outside `0..=1` is refused rather than clamped: a rule stored at the wrong threshold mis-tags mail with nobody looking, and clamping would store one nobody asked for.
- **note (what a tag application reports):** a row per message, not a count — a tag that applied to four of five and failed on the fifth is the outcome worth seeing. Applied sequentially, because these reflect to IMAP and fanning a selection out into simultaneous STOREs is the "500 concurrent IMAP mutations from one keystroke" `MAX_BULK` exists to prevent; a failure on one message does not abandon the rest. `:tag suggest` is the one streamed report here that *appends* rather than replacing, because `SuggestTags` sends each suggestion once — `SearchService.Search`'s discipline, not the finder's.

## 96. AI policy, safety and audit commands
- [x] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - `AiPolicyService` (4), `AiSafetyService` (2), and `AuditService` (2) wired behind `:ai budget status|set`, `:ai provider status|set`, `:ai scan`, `:ai audit`.
  - `:ai budget set` with no arguments opens the Settings-style form pre-filled from `GetSpend` rather than issuing a partial `SetBudget` (which would clear unset caps); flags pre-fill the form; a trailing `!` applies immediately with CLI replace-semantics. Spend renders against caps with soft/hard color *and* glyph.
- **verify:** `cargo nextest run -p rmail-cli tui::commands::ai_policy` (bare budget-set opens prefilled form, bang applies immediately and clears unset caps, soft/hard glyph thresholds)
- **note (the form arrived here, five tasks before the screen that needs it):** the acceptance says "Settings-style form", and task 101 is the settings screen — so the choice was to fake one for this verb or build the real thing early. `tui::form` is the real thing: `FormPane` holds fields, a cursor and one open edit, and applying it *rebuilds a `:` line* (`FormPane::line`) and hands it to `command::parse`. That last part is the property task 101 needs and the reason it was worth building now — a keypress produces an `Invocation`, so a settings field is testable by asserting the line it would run with no daemon anywhere near it, and a form cannot do something a typed line could not. `tui::form::tests` is 18 tests about the pane with no verb in them.
- **note (no new mode for it):** navigating fields is `Mode::Menu` and editing one is `Mode::Insert`, derived from `FormPane::editing` exactly as the search pane derives `Prompt` from `SearchPane::typing` — so `j`/`k`, `<enter>`, `<bs>` and `<esc>` already mean the right things and `keys.toml` needs no new layer. `Mode::Settings` stays task 101's, for a full screen with its own chain; a transient overlay does not need one. `<esc>` closes the innermost thing: the edit first (putting the value back), then the form.
- **note (why `SetBudget` cannot simply be sent):** it replaces a scope's whole budget — a cap the request omits is a cap *cleared*, which the proto and `mail ai budget set` both say outright. So `:ai budget set --daily-hard-usd=5` typed against a budget that already had a monthly cap would silently delete it. The bare verb therefore issues `GetSpend`, opens a form pre-filled with what came back, and applies *every* field; flags on the line pre-fill it further and win over the daemon's values; `!` skips the form and sends exactly what was typed. `a_bare_budget_set_applies_every_cap_the_daemon_reported` and `a_banged_budget_set_sends_only_what_was_typed` are the two halves, and reverting either behaviour fails them.
- **note (an unfilled form must not replace what it could not read):** `FormPane::ready` is false until the pre-fill lands, and `FormPane::blocked` refuses to apply while it is — including when the read *failed*, which is the case that matters: a form that could not see the caps in force is exactly the form that must not replace them. Stated on the pane rather than at the keypress, so it is one rule and testable without a `Model`.
- **note (the line carries what the fields do not own):** `--account` and `--bulk` choose *which* budget is being replaced, and a rebuilt line that dropped them would replace the global one however the form was opened — a wrong answer that looks exactly like the right one. `line()` therefore emits the invocation's positionals, then every flag no field owns, then the fields. Values are quoted the way `tokenize` reads them back, because a field takes whatever was typed into it and a space would split one value into two tokens.
- **note (refused where it was typed):** `edit_or_apply_form` asks `commands::answer` *before* closing the form and keeps it up on `Answer::Refused`, so a field holding `abc` is refused with that field still on screen and still editable. Free to do because the answer table is pure — no overlay, no request, no `Model` — so asking it twice cannot drift from the answer that runs.
- **note (a cap that is not a cap):** `overlays::truncate_chars` appends its ellipsis *past* the limit it is given, which is right for a table cell (the column reserves the room) and wrong for a field, where the same number is what `push` refuses at — a value arriving one character over `MAX_VALUE` could never be edited back into bounds. `form::bounded` keeps the marked cut but fits it inside the cap. The same shape of bug as task 92's `fitted`, in a different place, found by asserting the cap rather than by reading.
- **note (eight rows, not four):** `:ai budget status` draws a row per class, window *and* dimension. The row's tone is the point of the report — "am I about to be throttled" — and a scope over its soft token cap while under its dollar cap is two answers that one row would have to report as one. `wire::cap_state` is `spend_health`'s own ladder (hard before soft, because a scope past both is blocked rather than downgrading; no cap at all is muted, since unlimited is a configuration and not a warning), so the report and the status bar cannot disagree.
- **note (two verbs the acceptance's list does not name):** `:ai confirm` reaches `ConfirmInjection` and `:ai audit --all` reaches `ExportLedger` — both are RPCs the acceptance counts and neither has a spelling in its list. `:ai confirm` has to be a verb because the scan report's `actions` row carries it, and a row's action *is* an `Invocation` (task 90). It is the one row here that is **not** bang'd: releasing a safety hold is consent to AI-decided changes to mail, and the proto is explicit that consent a machine can grant itself is not consent — so task 90's `[y/N]` gate firing on exactly that row is the gate earning its keep. Confirming rescans first, as `mail ai scan-injection --confirm` does, because a confirmation is consent to a specific set of findings.
- **note (zero means opposite things to two verb families):** for `:ai budget *` and `:ai provider *`, account 0 is the *global* scope, so the scope is read from `--account` and never inferred from whatever mailbox is open — a spending cap silently written against the account on screen is the kind of surprise a spending cap must not have. For `:ai audit`, 0 means *every* account, so it is sent as an absent filter rather than as a literal 0 the ledger would match nothing against. Both are asserted.
- **note (`AuditService`'s first surface anywhere):** it has no CLI verb (`parity` records `cli: []`), so `:ai audit` is the first place a human can read the ledger — the same position `:rule *` was in at task 95, and the manual says so and points a script at `mail api call AuditService.QueryAiCalls`.
- **closed at merge — a task-94 fixture that named this task by name:** `commands::tests::a_verb_this_build_has_no_answer_for_is_not_a_refusal` built `:ai budget status` by hand as its stand-in for an unanswered verb, with a comment saying it was "the exact shape task 96 will arrive in". It arrived, and the test failed. Now pinned to a path the registry does not declare, which cannot be overtaken by a later task and tests the same fallthrough. `status::tests::the_hint_is_the_verbs_own_canonical_spelling` reserved its fourth hint as `None` for the same reason and now names `:ai budget status` — no edit in `tui::status` made that happen, which is what the derivation was for.

## 97. Accounts, sync control and tokens
- [x] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - `Model.accounts: Vec<Account>` added alongside the existing single `account` field; `:account use <id>` switches the active account within a session (previously explicitly deferred).
  - All nine `AccountService` RPCs and all three `AdminService` RPCs wired behind `:account list|show|add|login|refresh|test|rm|use` and `:token list|create|revoke`; `Autoconfigure` output and OAuth URLs render in a Report with copy/open affordances (reusing the existing `html::CommandOpener`); a minted token secret is shown exactly once with an unrecoverable marker.
- **verify:** `cargo nextest run -p rmail-cli tui::commands::account tui::commands::token` (in-session account switch, OAuth URL open path, token shown once and not recoverable from subsequent state)
- **note (thirteen verbs for twelve RPCs, and why the counts differ):** `:account login` is two RPCs — `BeginOAuth` binds the loopback port and returns the URL, `CompleteOAuth` blocks until the browser comes back — and a client that issued only the first would leave a port held for a flow nobody could finish, so they are one command. Against that, three verbs reach no RPC at all: `:account use`, and `:account toml`, and neither is optional. `:account use` is the acceptance's in-session switch; `:account toml` is the copy affordance, and it is a *verb* rather than a row-only gesture because a report row's action **is** an `Invocation` (task 90) — an affordance only a row could reach would be one nobody could type, and the verb registry is also the command index, so it would document nothing either. Both are hand-written in `run_invocation` next to `:set`, for the reason that function's own comment gives: the id and the state they read are not something an `Action` can carry.
- **note (`:account add` proposes, `:account new` writes):** `Autoconfigure` is explicit that it "writes no account, touches no existing one, and returns a proposal for a human to apply", and on a miss the proposal can come from a model. So `add` probes and reports; `new` is what stores. Two verbs rather than one switching on the presence of a flag — the `:ai budget set` shape was considered and rejected here, because there the two destinations were one RPC drawn two ways, while these are two different acts and a line that created an account when it was expected to propose one is not recoverable. `new` rather than `create` to sit beside `:tag new` (`CreateTag`); `AccountCreate` has no CLI verb at all, so there was no spelling to inherit and this is its first surface anywhere.
- **note (the apply row is a `:` line, built flag by flag):** `wire::new_account_invocation` assembles `account new <email> --imap-server=… --imap-port=… --username=…` from what was discovered and parses it, so the row runs exactly what typing that line runs and the settings are visible before it does. Not bang'd, unlike task 95's suggestion rows: creating an account is the mutation task 90's gate should ask about, and a proposal that may have come from a model is precisely when `[y/N]` in front of the settings earns its keep. The row is withheld entirely when `existing_account_id` is set — `Create` would make a second account for that address, and the report already says which one exists.
- **note (`command::quoted`, and why it belongs in the parser's crate):** a discovered username comes out of an autoconfig document fetched over the network. Unquoted on a rebuilt line, one containing a space splits into two tokens and asks the verb about something nobody typed. The escaping is the exact inverse of `tokenize`'s, so it lives next to it as `command::quoted` with a round-trip test — and task 96's `tui::form` now uses that instead of its own private copy, which was the second copy of the rule that would have drifted.
- **note (the one verb that will not guess):** every account verb falls back to the account on screen — "this account" is what somebody means when they are looking at it — *except* `:account rm`, which cascades to every message stored for the account. A line that deleted whatever happened to be open because its id was left off is a line nobody should be able to type by accident, so it refuses and names the listing. It is also the only account verb that asks, which is the same per-verb judgement `Request::confirm` was introduced for and not `effect()`.
- **note (switching is one path, not two):** `use_account` clears the folders, the message rows, the open message, the analysis panel, the cursors, the scroll offset and the visual selection, drops the undo toast, returns to the list screen, and then issues exactly what `Msg::Accounts` issues when the first account loads. An id the daemon has never listed is refused rather than sent: `LoadFolders` for an account that does not exist answers `NOT_FOUND` two round trips later, by which point the screen has already been cleared for it. Switching to the account already open does *nothing* — not even a reload — because throwing away somebody's cursor and open message to fetch the same rows is the opposite of what they asked for.
- **closed at merge — one open stream per switch:** `Cmd::Watch` used a plain `tokio::spawn`, which was correct while an account was chosen once at startup. Re-issuing it per switch would have left one live `WatchEvents` stream per switch, each still sending `Msg::Changed` for an account nobody was looking at. It now goes through a superseding `watching` slot, like the heartbeat. Found by asking what the four startup commands do when issued twice, not by a test failing.
- **note (a range cannot reach `:account use`, and the row is why that matters):** opening `:` over a visual selection prefills `'<,'>`, and `unsupported_range` refuses it for a verb that reaches no capability — so the typed path can never run this with a selection up. The listing row can, since a row's invocation carries no range, which is exactly why clearing the selection is real work and not defensive tidying. The test drives the row path for that reason.
- **note (the secret is a row and nothing else):** `MintToken` returns the bearer secret once — only an argon2id hash is persisted, so the daemon *cannot* show it again. It therefore lives in the report's rows and nowhere else, and the marker row underneath says so outright, because a reader who does not know will close the pane. `history::is_secret` has refused to record `token …` lines since task 89 and the verbs it was written for have now arrived, so that is asserted rather than assumed. The strongest test formats the whole `Model` after the pane closes and refuses to find the secret anywhere in it — a field somebody adds later for a `:token show` would fail it.
- **note (`r` had to learn to refuse):** `r` means "ask this verb again", and asking `MintToken` again mints a second token. `ReportPane::once`, declared per verb through `Request::once`, is the narrow case: not `effect().is_mutating()`, since `:sync now` mutates and re-running it is what `r` is *for*, but a verb that **produced** something a second run would produce again. `:token create` is the only one today.
- **note (copy and open, without a clipboard dependency):** the acceptance asks for copy/open affordances reusing `html::CommandOpener`. Opening is literal — `html::open_url` hands the authorization URL to the platform opener, `https` only and refused rather than trusted, since the argument goes to whatever handler is registered for its scheme. Copying is the same mechanism the HTML viewer uses: `html::open_text` writes the block to a `0600` file under the pid-scoped prefix `sweep` cleans up, and opens it in whatever handles `.toml`. There is no clipboard crate in this workspace and adding one is a platform matrix for one row.
- **note (the URL is drawn even when a browser is about to get it):** `CompleteOAuth` blocks until a human consents, so the report sends its first frame *before* that call — a report showing nothing until the flow finished would withhold the one thing the human needs to act on. A launch that fails appends a row saying so and the flow continues; `--no-browser` skips the launch. No client-side deadline on the second call either: `RPC_TIMEOUT` would abandon the flow while somebody was still reading a consent screen, and the daemon's own port expiry is on screen.
- **note (what "sync control" already was):** the acceptance's title names it, and `:sync now|pause|resume|status` shipped in task 94 — all four `SyncService` RPCs, tested there. Nothing was added for it here, and nothing was missing.

## 98. Automation and notifications
- [x] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - `WebhookService` (7), `HookService` (2), and `NotificationService` (2) wired behind `:webhook list|add|rm|enable|disable|deliveries|replay`, `:hook list|test|add`, `:forward`, `:notify list|score|set`.
  - `:hook add` and `:notify set` follow the `ReadOnlyReason::ConfigFileOnly` presentation established in task 101's field model — the exact TOML block to paste, its path, and a copy affordance — never a fabricated write RPC. `:notify list` renders `StreamAlerts` live in a Report.
- **verify:** `cargo nextest run -p rmail-cli tui::commands::automation` (webhook CRUD + replay, hook test round-trip, config-block presentation for hook/notify config-only fields, live alert Report)
- **note (`ReadOnlyReason::ConfigFileOnly` arrived here, three tasks before the screen that names it):** the acceptance points at task 101's field model, and task 101 is not built — so the choice was to fake the presentation for two verbs or build the real thing early, exactly as task 96 faced with the form. `tui::config_block` is the real thing: `ConfigBlock` plus a two-variant `ReadOnlyReason`, and `rows()` is the whole presentation. Task 101 adopts a type that already has callers instead of inventing a second one.
- **note (two reasons, not one, because the difference is what a reader needs):** `ConfigFileOnly` is hooks and notification thresholds — nothing anywhere writes them over the wire, so the config file is not "the other way", it is the only way. `AlsoOverTheWire(verb)` is accounts, where `:account new` stores one through `AccountService.Create` and the block is the alternative; it carries the verb so the row can name it. Collapsing them would either hide an available verb or imply one that does not exist.
- **note (why the TUI shows the block and the CLI writes it):** `mail hook add` reads the config file, appends, round-trip validates and renames, then exits — correct for a one-shot command. A TUI holding the same file open across a session has no idea what else has edited it since startup, and the daemon it is talking to has already loaded its own copy. So `:hook add` and `:notify set` render; the block is the same text either way, and `rmail_core::config::toml_string` is now the one escaper both use (it was private to `hook_cli` — two copies of an escaper is two chances to disagree about what a quote does).
- **note (one `:toml` verb, replacing task 97's `:account toml`):** three verbs now produce a block, and three verbs for opening one would be three names for one idea. `Model::block` holds the newest and `:toml` opens it — a verb rather than a row-only gesture, because a report row's action *is* an `Invocation` and an affordance nobody could type would document nothing either. Task 97's `:account toml` was renamed to it in the same change rather than left as a fourth spelling.
- **note (`Answer::Block`, and the dispatch bug it exposed):** a verb that reaches no capability had no route to the answer table — `run_invocation` tested `capability.is_some()`, so `:hook add --name=x` fell through to the generic "not wired up yet" and was refused for carrying flags nothing had read. The condition is now `action.is_none()`, and `run_daemon_command` is renamed `run_answered_command` because it is no longer only about the daemon. Found by writing the dispatch test, not by reading.
- **note (`:webhook` is the only place mail leaves the machine, and it is spelled that way):** the URL is shown as `scheme://host` unless `--reveal-url`, because a webhook URL is frequently the credential itself — the same reason `List` redacts it on the wire. `--include-body` is a property of the destination and its row is drawn as a *warning*, since it ships the mail itself to a third party on every matching message. The signing key is a reference (`--secret-env`/`--secret-command`/`--secret-keychain`, at most one), and an unsigned destination says `unsigned` rather than implying a receiver can verify something.
- **note (queued, never sent):** `Forward` queues; the dispatcher sends on its next tick. The status line says so, and says it louder when `dispatcher_running` is false — a client that reported a send on a daemon with `webhooks.enabled = false` would be the lie that response field exists to prevent. In the queue view, `last_status == 0` renders as "no answer yet" rather than as an HTTP code, because nothing answering at all is a different operational fact from a 500.
- **note (only a failed delivery offers a replay):** replay is the only way out of the terminal state and deliberately something a human does, which is exactly what a row action is — but a *pending* row offering it would be inviting a second POST of something still on its way. Not bang'd either: it re-POSTs the same mail content to a third party, so task 90's gate asking first is the gate doing its job. It resends the frozen bytes under the same delivery id, not a fresh render of a mailbox that has since changed.
- **note (`:webhook rm` is the second verb in this vocabulary that asks):** it deletes the destination *and its delivery history* — the record of what already left this machine — and the question names `:webhook disable` as the reversible answer rather than leaving somebody to discover it afterwards.
- **note (`:forward` was free):** `Action::Forward`'s id is `message.forward`, so its auto-derived verb is `:message forward` and the bare path was unclaimed — no `:reply`-style branch was needed. `--to` is required, which is what `mail forward <id> --to …` requires too.
- **note (the live feed never completes):** `:notify list` is the one streaming report here with no end. It appends and never sets `complete`, so the border keeps saying it is listening, which is true; `Esc` stops it through the same `reporting` slot every other stream uses. A stream the *daemon* closes is reported as a failure rather than as completion — a live feed that silently stopped would leave somebody watching a pane that can no longer tell them anything.
- **note (`:notify score` explains a silence):** the interesting answer is rarely the tier. `state`, `threshold`, `account` and `suppressed` are all rows, because "we chose not to" (SUPPRESSED) and "we could not" (FAILED) are different facts the proto keeps apart, and "queued" means scoring was asked for and runs through the AI queue's policy/redaction/budget/audit gates rather than blocking this call.
- **note (a threshold outside the ladder is refused where it was typed):** the proto is explicit that an unrecognised tier delivers *nothing* and only warns at daemon startup — so a typo pasted into the config file is notifications silently switched off, discovered weeks later. `--enabled`/`--disabled` (and the subject/reason pairs) both exist because these render TOML rather than sending a request: a flag that could only be turned on could never write `include_subject = false`.
- **closed at merge — two confirmations nobody was asserting:** `commands::tests`' sweeps build a placeholder argument per declared positional, and used the positional's *name* — so every id-taking verb answered `Refused` in every sweep and was quietly excluded from what they claim to cover. `with_arguments` now passes a number for an id-shaped positional, which turned "rebuild is the only verb that asks" into the true list of five (`index rebuild`, `account rm`, `webhook rm`, and task 100's `draft delete` and `outbox send-now` — the last two had been unasserted since they were written).
- **closed at merge — the WhichKey band has outgrown its cap:** `whichkey::tests` named two verbs as examples of a group and a leaf; the `:` vocabulary now exceeds `MAX_ENTRIES`, so the leaf example (`quit`) was trimmed out of the middle and the test failed for a reason that had nothing to do with what it was testing. Rewritten to assert the classification over whatever the band *did* show, which gets stronger as the vocabulary grows rather than more brittle.

## 99. Content, export and analytics commands
- [x] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - `ExportService`, `AnalyticsService` (5), `AttachmentService` (4), `ExtractService` (3), `LinkService`, `NoteService` (5), `SavedSearchService` (11), and the untouched `SearchService` methods (`CompileQuery`, `SearchAttachments`, `SearchEntities`, `Evaluate`) wired behind `:export`, `:digest`, `:stats response-time|ask`, `:contact`, `:subs`, `:attach list|tables|invoice|ask|search`, `:extract events|tasks|data`, `:links`, `:note add|list|edit|rm`, `:saved list|save|run|rm`, `:folder new|list|members|eval|rm`, `:search compile|attachments|entities|eval`.
  - `:digest` rows open their cited source message on Enter.
- **verify:** `cargo nextest run -p rmail-cli tui::commands::content` (export streams to each format, digest row citation navigation, saved-search/smart-folder CRUD, attachment ask/search)
- **note (thirty-seven verbs, four sub-tables, one filter):** `tui::commands::content` is a dispatcher over `analytics` (the reports plus the export), `attach` (what is attached and what is inside it), `extract` (what a message contains, plus the three verbs that read the *index* rather than the mail) and `library` (the things a user names and keeps). Four modules because thirty-seven verbs in one file is not reviewable; all under `content`, which is the acceptance's own verify filter, so it selects every one of them.
- **note (five verbs the acceptance's list does not name):** `:attach invoices` is `ExportInvoices`, spelled as `mail invoices` spells it. `:note watch` is `WatchNotes`. `:saved edit` is `UpdateSavedSearch`. `:folder compile` is `CompileSmartFolder`. And `:message open <id>` is the one that mattered most: the acceptance requires digest rows to open their cited source, a row's action **is** an `Invocation` (task 90), and nothing in the vocabulary could open a message *by id* — `Action::Open` is the key that opens what the cursor is on. Four report families now cite messages (a digest line, an attachment hit, a saved search's hit, a smart folder's member) and all four reach that one verb.
- **note (a window is a duration typed and an instant sent):** `--since 30d` is what `mail stats` accepts; the RPCs take absolute seconds, because a report has to name the window it summarized and a relative bound would mean something different by the time it was read. The conversion needs a clock and `update` is pure, so the `Cmd` carries "this many seconds back" and the wire seam subtracts it from `now` — the same split `Msg::Tick` exists for. `--until` is absolute, because "until 30 days ago" is a window nobody asks for.
- **closed at merge — a free-text verb read its own argument as an id:** `content::message` reads the first positional as a message id, which is right for `:links 42` and wrong for every verb whose positionals are what somebody *wrote*. `:attach ask what is the total` and `:note add chased this` were both refused with `"what" is not a message id`. Split into `message` (verbs declaring an id) and `on_screen` (verbs declaring text), and pinned by a test that walks the registry rather than naming the two verbs — so a free-text verb added later is covered without an edit.
- **note (the model call is opt-in on every verb that has one):** `:attach tables --model`, `:attach invoice --model`, `:extract * --model`, `:links --model`, `:digest --force`, `:contact` *without* `--metrics-only`, `:subs --classify`, `:stats ask --narrate`. Each costs money, and a report that spent it by default is a report somebody discovers on an invoice. A test asserts each switch changes the request rather than being decoration.
- **note (provenance is a column, not a footnote):** an invoice field says `parsed` or `model`, because a total a parser read out of a text layer and a total a model inferred from a scan are not the same claim — and a report that flattened them would be inviting somebody to pay the second one. Same rule for an extracted event (inferred from prose is drawn `Warn`, a cancellation `Bad`), for a table (`inferred by a model`, `truncated`), for a compiled query (`a cached compilation` vs `a fresh model call`) and for an eval query with unresolved judgments (every metric for it is a lower bound, not a measurement).
- **note (`:export` reuses the writer, and `--to` is required):** turning framed bytes back into an mbox, a Maildir or a directory of `.eml`s is `rmail_core::export::write::DestinationWriter`'s job — the shared code that also owns the check keeping a server-supplied entry name inside the named directory. One blocking task fed by a bounded channel, not one `spawn_blocking` per frame, so a slow disk throttles the daemon's scan instead of letting this process buffer an archive it has not written. A partial archive is left on disk in every failure path and the report says it is incomplete; deleting a half-written export would destroy the only copy of what did arrive. There is no default destination: a verb that wrote somewhere the user had not named would be the worst possible default.
- **note (`:search eval` is the one verb that reads a file, and why that is not task 95's mistake):** `Evaluate` takes its judgments *by value*, deliberately, so the daemon needs no filesystem access to the client's directory — and a golden set exists nowhere but on disk. Task 95 rejected a file path for `:rule add` because a better flow existed (draft, read the dry run, store); here there is no flow that avoids the read. It happens on a blocking task through `rmail_core::eval::GoldenSet`, the same parse `mail search eval` performs, so a malformed set is refused with a message about a path the user can see.
- **note (two verbs, not one flag, twice):** `:saved save`/`:saved edit` are `Create`/`Update`, which refuse opposite things — an upsert would store a typo'd name as a new entry. `:folder new`/`:folder compile` take a predicate and a sentence, and one of them spends money at a provider; that is not a difference to hide behind whether a flag was given. `:attach search` and `:search attachments` are the *same* verb under two paths, which is the `:helpgrep`/`:manual grep` precedent: the thing belongs to two families at once.
- **note (`SavedSearchService` has no CLI at all):** eleven RPCs, and `:saved *` is their first human surface anywhere — the position `RuleService` was in at task 95, so these spellings are what a future `mail saved` has to adopt. The smart-folder half does have a CLI (`mail folder …`) and follows it exactly.
- **closed at merge — the command palette ranked a buried substring first:** typing `arch` began returning `attach search` above `message archive`. Both merely "contained" the needle, so the tie broke alphabetically. `command_matches` grew a word-start tier between prefix and substring, which is the honest fix — which verbs collide that way depends on what the registry holds, so it cannot live in whichever example a test picked. The old test's example (`quit`, then `help` before that) had already been overtaken twice.

## 100. Compose, send and follow-up commands
- [x] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - All ten `ComposeService` RPCs and the remaining `SendSchedulerService` methods wired behind `:reply [--ai]` (streaming `DraftReply`), `:draft list|show|rewrite|revisions|revert`, `:send [--at][--undo]`, `:outbox`/`:outbox cancel|retry|reschedule`, `:followup list|new|dismiss`, `:waiting`, `:nudge`, `:preflight`.
  - The existing undo toast remains the only countdown surface (no second one introduced for the command path).
- **verify:** `cargo nextest run -p rmail-cli tui::commands::compose` (AI reply streams to an editable draft, send/undo window unchanged, follow-up lifecycle round-trips)

## 101. Settings screen
- [x] status
- **depends-on:** 95, 96, 97, 98
- **parallel-safe:** no
- **acceptance:**
  - `Screen::Settings` and `Mode::Settings` (chain `[Settings, Global]`, restating j/k/gg/G/`<tab>`/`<enter>` rather than inheriting `Normal` — the same reason `Menu`/`Pick` already restate them). Reached via `:settings [<section>]`, the `<space>cc` leader chord (task 105), and an `s` key from any Report.
  - A `FieldKind` model (`Toggle`, `Choice`, `Number`, `Text`, `Run`, `ReadOnly{ConfigFileOnly|NoRpc}`) where **every field's write is expressed as a `:` command `Invocation`** — the screen has no private path to the daemon, so it is testable by asserting the invocation a keypress produces, with no daemon required. `ReadOnly::ConfigFileOnly` fields render the exact TOML block, its path, and a copy affordance. Settings › Keys writes through `rmail_core::keymap::file::edit` directly, not `ConfigService.SetBinding`, so rebinding still works with the daemon down.
  - Sections: Accounts, Sync, Index, AI, Safety & audit, Rules, Tags, Automation, Notifications, Saved searches, Keys, Interface, Tokens, Daemon.
- **verify:** `cargo nextest run -p rmail-cli tui::settings` (every field's keypress produces the expected Invocation with no daemon connection, config-file-only fields render their block, Keys section bypasses gRPC)
- **note (the field model was already half-built, twice):** task 96 built `tui::form` for `:ai budget set` and task 98 built `tui::config_block` for `:hook add`/`:notify set`, both under the same reasoning — the real thing early beats a stand-in this screen would have had to replace. So `FieldKind::Number` *is* the form (a number is typed into the form its line opens) and `ReadOnly::ConfigFileOnly` *is* the block (its line renders it, and a test asserts that line really answers with a `Block` rather than merely being called one). Nothing new was needed for either.
- **note (the screen shows switches, not values, and that is the design):** a toggle here does not know whether the thing is on. Asking would mean a read per field on every open, a value stale between reads, and a screen that could not be tested without a daemon — three costs for a convenience already covered better: every section's first field is the *report* that answers "what is it now", and that report knows how to draw a soft cap differently from a hard one. A section therefore reads as "here is the state, and here are the switches".
- **note (`FieldKind::Text` is the one kind that runs nothing, and that is not a hole):** the acceptance says every field's **write** is an `Invocation`. An address, a token label, a chord and a query are things only the person at the keyboard has, so a field for one has no write to express — it puts the verb on the `:` line with the cursor after it and lets them finish. That is a real affordance rather than a gap, and `every_text_field_names_a_real_verb` still holds the line to the registry.
- **note (three tests over the whole screen, not one per field):** `every_line_parses` refuses a field whose line the parser rejects — a field that opens, accepts a keypress and *then* refuses is the worst of the three outcomes. `every_line_is_answered_by_something` asks `commands::answer` directly rather than inferring from the invocation's capability, because task 98's block verbs reach no capability and are answered. `every_field_that_runs_something_can_say_what` is the acceptance's own property over all fourteen sections at once. A field added later is covered by all three without an edit.
- **note (`<tab>` is `focus.toggle`, not an action of its own):** it means "the next thing over" and what that is depends on the screen — the other pane on the list, the next section here — which is the shape `cursor.down` already has. It also had to be: `settings.section` as an id auto-derives a `:settings section` verb, and that shadows `:settings <section>`, which `command::tests::no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one` refuses and rightly. The test caught it before a reader would have.
- **note (`Mode::Settings` restates rather than inherits):** a screen of fields that fell through to `Normal` would answer `a` with "archive" and `d` with "delete" over rows that are not messages. `Mode::Settings`' chain is `[Settings, Global]`, and the test that pins it focuses the message list first — otherwise `archive` refuses for want of a selection, which looks exactly like the key being unbound and would have passed under a broken layer.
- **closed at merge — `MODE_WIDTH` claimed to be derived and was not:** its docs said "computed from the longest `Mode::id`" and the value was a hardcoded `13`. An eleventh layer whose label is fourteen columns wide would have quietly shifted every zone after it. It is now literally computed in a `const fn`, so it cannot go stale again.
- **closed at merge — six tests were asserting list contents rather than behaviour:** adding a layer and a `g` chord broke `every_mode_has_its_own_label` (a hardcoded `10`), `tab_and_ctrl_o_cycle_the_key_references_mode_both_ways` (named `Help` as the last configurable mode), two key-reference tests (counted `<c-o>` presses to reach `Help`), `the_band_reads_the_mode_the_model_is_in` and three continuation tests (asserted the exact set of chords under `g`), and `a_binding_that_would_make_another_unreachable_is_refused` (unbound one shadowed chord where there are now two). Every one is now derived from the list it was really about, which makes each *stronger*: the shadowing test asserts every chord `g` kills rather than the first, and the continuation test asserts what each `g` chord leads to.
- **note (`:set theme` arrived with the screen that needed it):** `Model::theme`'s own docs anticipated it by name — the theme lives on the model rather than being a parameter `view::render` takes, so switching it is an ordinary state mutation. `ThemeName` and its four themes already existed and were selectable only at startup. Interface › theme is the `Choice` field over them, and `set_option`'s docs asked for exactly this ("when it lands, this is where a new `Invocation` it wants to issue should keep landing").
- **note (what task 105 still owns):** the acceptance names `<space>cc` as a third way in and defers it to task 105's leader map. `gs` from the list and `s` from any Report are here; the leader chord is that task's.

## 102. Help overlay redesign
- [x] status
- **depends-on:** 91
- **parallel-safe:** yes
- **acceptance:**
  - `?` becomes mode-aware (renders `Model::mode()`'s actual chain at the moment it was pressed, with `<tab>` cycling to other modes), scrollable (no silent truncation past the terminal height), grouped by the same derived id-prefix grouping WhichKey uses, and filterable with `/` — a subsequence match over id/chord plus a substring match on the description, the same fields `command_matches` ranks by but as inclusion only, not its four-tier reordering (see the note below).
  - No longer a dead end: `<enter>` on a row runs the action, `c` opens `:keys set <chord> <action>` pre-filled, `K` navigates to that action's manual page (task 103).
- **verify:** `scripts/docker-test.sh tui::help`, `scripts/docker-test.sh 'tui::model::tests::.*help'` and `scripts/docker-test.sh 'tui::view::tests::.*help'` between them (mode-chain rendering per invoking mode, scroll past terminal height, filter matches, row actions run/rebind/navigate correctly) — three commands because the tests are split across those three module paths and `cargo nextest`'s filter matches a test's *name*, not which module owns it, the same trap task 90's own note records.
- **note (filter is a match test, not a tiered ranking):** the acceptance originally said `/` would reuse `palette_matches`' (now `command_matches`') tiers; what shipped is `is_subsequence(needle, primary) || describe.contains(needle)` — a match/no-match test, not a *ranking* that reorders results by tier (prefix beats substring beats subsequence beats description). As boolean inclusion the two are equivalent — `starts_with` and `contains` both imply `is_subsequence`, so `command_matches`' first three tiers accept exactly what one subsequence check does — the gap is only that this filter never reorders a bucket's rows by match quality; they stay in `Action::ALL`'s declaration order within a shared id-prefix. Real tiering would also have to interact with the id-prefix grouping this same acceptance requires (a tier boundary crossing a group's own header is not obviously the right reading), which is a design question of its own rather than a one-line fix, so it is left open rather than shipped half-considered.

## 103. Manual engine
- [x] status
- **depends-on:** 88 (102 relaxed — see the note below)
- **parallel-safe:** no
- **acceptance:**
  - `Screen::Manual` (reusing `Mode::Help`) with a page registry of `include_str!`-compiled markdown (works with the daemon down and with no filesystem access), a deliberately tiny renderer (headings, bullets, fenced code, `[[anchor]]` links, `{{keys:…}}`/`{{cmd:…}}`/`{{capability:…}}` expansions — nothing else), a back/forward stack (`<c-o>`/`<c-i>`), in-page `/` search, and `:helpgrep [<pattern>]` opening the cross-page hit list.
  - Generated sections (key reference, command index, capability footer, mode/layer diagram, unbound-actions list) read the live `Keymap`, the verb registry (task 88), and `parity::Command` — never stored as prose.
  - Reconciliation tests: every registry verb has a page anchor; no dangling `[[anchor]]`; every `{{…}}` expansion resolves (fails the build, not the render, when it can't); every `Action::ALL` id is documented somewhere.
- **verify:** `cargo nextest run -p rmail-cli tui::manual` (anchor/link/expansion reconciliation tests, back-stack navigation, helpgrep produces its hit list, works with no daemon connection)
- **note (dependency relaxed):** written before 89/90/102 rather than after. Only one clause of the acceptance actually needed them — `:helpgrep` typed on a command line that does not exist yet, rendering into a `Overlay::Report` that does not exist yet — and everything else (the registry, the renderer, the expansions, the navigation stack, the in-page search, the whole reconciliation suite) is self-contained. `manual::grep` is therefore a pure `(pattern, &Keymap) -> Vec<GrepHit>` function, which is the part task 90's Report consumes; where those rows are *drawn* today is `Location::Grep`, a generated manual page reached by `Action::ManualGrep` (`g/`), so the feature is reachable and tested rather than stubbed. Task 89 dispatches `:helpgrep <pattern>` (or `:manual grep <pattern>` — both paths are declared, both carry the same action) into `model::open_manual_grep_for`, which exists and is tested: 89 wires an argument-carrying verb to it rather than delegating to `run_action`, whose signature takes a count and cannot carry a string. `model::open_manual_at` is the same seam for a page name, which is what task 102's `K`-on-a-row needs. Task 90 re-points the presentation (one call site) if a Report reads better than a page. Task 102's `K`-from-a-help-row is 102's own acceptance, not this one; `manual` is bound to `K` in Normal/Viewer/Help so the manual is reachable now, and 102 refines Help's `K` to be row-aware.
- **note (acceptance amended twice, both recorded above):** `:helpgrep`'s pattern is *optional* rather than required — a bare `:helpgrep` opens the same prompt the `g/` binding does, which is both more useful than a `MissingPositional` error and the only spelling reachable before task 89. And `keys`/`commands`/`modes`/`capabilities` are *generated* pages here, not prose: task 104's "reference pages" list names all four, but Part V's own ground rule ("Help stays generated from data, never hand-maintained") puts them on this side of the line. Task 104 authors the rest of that list (`keys-toml`, `config-file`, `troubleshooting`) and everything else in its acceptance.
- **closed at merge:** review (re-deriving each acceptance clause against the code rather than the comments) found six real defects, all fixed before this counted as done. Three were shipped behaviour: every hard-wrapped bullet on both authored pages rendered its continuation as a *separate flush-left paragraph* between two bullets (the parser had no lazy continuation, and both pages are written in that style, as task 104's forty will be); `targets` consulted the visual selection before the screen, so `message.archive` rebound into the `help` layer archived the rows underneath a page of prose, with the list not even drawn; and `n`/`N` painted a red "nothing searched for yet" over the status line when pressed in the `?` overlay, which shares `Mode::Help` and has no page to search. Three were tests that could not fail: the manual-cursor clamp test asserted `0 < len` because `go` sets the cursor to 0; the match-styling render test read a row the cursor was on, whose own ink `List::highlight_style` overrides wholesale; and the three reconciliation checks matched substrings, so `help` was satisfied by `helpgrep` and `manual` by `manual.back` — either could have vanished from the key reference with every check still green.
- **note (two pre-existing surfaces changed, deliberately):** `Model::selection` now returns `None` on any screen but the list, whatever `Model::visual` holds — the anchor still outlives leaving the list (so a selection survives reading the manual, or opening a hit found mid-selection), but the *range* does not, which is what stops a bulk action from acting on rows that are not on screen. That also changes `Model::mode`: the viewer reached from a search hit made mid-selection derives `Mode::Viewer` rather than `Mode::Visual`, so it no longer shows `-- VISUAL --` and `o` there is `message.open-html` rather than `visual.swap-ends`. Both are pinned by `a_selection_made_on_the_list_does_not_act_from_the_viewer_either`.
- **note (no inline emphasis, on purpose):** the renderer has no inline `` `code` ``/`*bold*` form at all — "nothing else" is honoured literally. Naming a key, a command or a capability inside prose is what `{{keys:…}}`/`{{cmd:…}}`/`{{capability:…}}` are *for*, and those are checked against the live registries by the reconciliation suite, so the styled-inline path and the correctness path are the same path. Authored prose that wants a literal backtick gets one, verbatim.

## 104. Manual content
- [x] status
- **depends-on:** 103
- **parallel-safe:** yes
- **acceptance:**
  - ~40 pages authored under `rmail-cli/src/tui/manual/pages/`: getting-started (`start-here`, `tour`, `modes`, `daemon`, `offline`); concept pages (`search-vs-finder`, `saved-vs-smart`, `archive`, `grounded`, `ai-cost`, `privacy`, `index`, `undo`, `bulk`); ~7 worked-example transcripts (triage-by-selection, rule-from-mistake, halve-the-ai-bill, add-oauth-account, find-the-clause, digest-to-slack, recover-interrupted-rebuild); best-practice notes, each stating its one-sentence reason; reference pages (`keys-toml`, `config-file`, `troubleshooting`).
  - `keys`, `commands`, `modes` and `capabilities` are **not** on that list: task 103 ships them generated from the live `Keymap`/verb registry/`parity::Command`, per Part V's "help stays generated from data" ground rule. `start-here` and `manual` also already exist (103 needed an index and a page describing its own navigation); extend them rather than replacing them.
  - Every capability with a TUI surface (per `parity::Command::actions()`) appears in **authored prose** — not merely in the generated capability page. This clause has to name the authored set explicitly, because 103's `every_capability_with_a_tui_surface_is_documented` scans every page including the generated one, which enumerates all 155 rows by construction: as written it would pass against forty pages that mention no capability at all. So 104's own work includes narrowing that scan (or adding its authored-only twin) and making it pass; `it_is_the_generated_index_that_makes_verb_coverage_hold` is the test that records which side of the line the coverage currently sits on, and it fails by name once authored prose covers everything.
  - Likewise `every registry verb has a page anchor` (103's acceptance) is currently "its canonical spelling appears on some page", not a verb→anchor mapping. Task 102's `K`-on-a-row needs the mapping — `open_manual_at` takes a page name and nothing derives one from an `Action` — so whichever of 102/104 gets there first declares, per page, which actions and verbs that page documents, and `Action`/`Verb` → anchor becomes a reconciliation test of its own.
- **verify:** `cargo nextest run -p rmail-cli tui::manual` (task 103's reconciliation suite passes against the full authored page set; zero unreferenced capabilities)
- **note (what was authored):** 39 new pages plus extensions to `start-here` and `manual` — 41 authored, 45 in `PAGES` with the four generated ones. Getting started (`tour`, `typing`, `daemon`, `offline`); concepts (`search-vs-finder`, `saved-vs-smart`, `archive`, `bulk`, `index`, `undo`, `grounded`, `ai-cost`, `privacy`); the seven named worked examples; sixteen `practice-*` notes, each an imperative followed by a one-sentence "Why"; reference (`keys-toml`, `config-file`, `troubleshooting`). `typing` is the one page not on the acceptance's list: the seven prompt/menu/pick/confirm/input actions needed a home that was not the tour, and `modes` — where they would otherwise have gone — is generated.
- **note (the mapping, declared not derived):** `Page` gained `documents: &'static [&'static str]`, and `manual::home_of` resolves an `Action::id` or a verb path to the page declaring itself its home. Declared rather than derived from each page's own `{{keys:…}}` markers because "the first page that mentions it" is not "where a reader should be sent" — `start-here` names nearly every action in its own summary, so a derivation would make the index the home of all of them. Three tests keep the declaration honest: every entry resolves to a real action or verb, every entry is actually cited by that page's own markers (a declaration cannot lie), and every action and verb has exactly one home (a bijection, both directions). `model::open_manual_at` now takes an anchor *or* an id, which is the seam task 102's `K`-on-a-row consumes; the anchor wins where a string is both, and `a_page_anchor_that_is_also_a_documented_id_resolves_to_its_own_page` is what fails when task 105's new vocabulary collides with a page name.
- **note (the capability scan was narrowed, and the verb tripwire was *not* removed):** `every_capability_with_a_tui_surface_is_documented` now scans authored pages only, matching `Service.Method` as rendered by a `{{capability:…}}` marker or by the page's derived footer — so all 13 TUI-surfaced capabilities are covered by prose that names a command reaching them, and deleting any one marker fails it. `the_generated_page_covers_every_capability` records the wider claim the generated page still carries. But this task's acceptance predicted `it_is_the_generated_index_that_makes_verb_coverage_hold` would "fail by name once authored prose covers everything", and it does not: authored prose names a verb with `{{cmd:…}}` (rendering `:message archive`) and an action with `{{keys:…}}` (rendering a *chord*), so the ~22 verbs a page only names by key still appear in no authored line. It was deleted on that false premise mid-implementation and restored with a docstring recording why it is still green. The acceptance clause was wrong, not the test.
- **closed at merge:** review (re-deriving each clause against the code, and against the real `mail` binary) found the deliverable's own content was its biggest defect: **17 of the fenced `mail …` transcripts did not parse or named verbs that do not exist** — `mail rule synthesize`/`backtest` and `mail attach ask` are RPCs with `cli: []` and no subcommand at all, `mail account test` likewise, and the rest were wrong flags (`--url` where a positional goes, `--daily-usd` for `--daily-soft-usd`, `--format mbox` for `--archive-format`, `--name` for `--destination`) or missing required arguments (`--account`, which is an `i64` id everywhere, not a name). `{{cmd:…}}` and `{{capability:…}}` are reconciled against the registries; a fenced shell line was prose the suite never read. The fix is structural: `every_shell_command_an_authored_page_shows_is_one_this_binary_accepts` walks `Cli::command()` — verb path, long and short options, option-vs-positional, and every required argument — so a renamed flag now fails the moment it compiles. The three pages whose subject has no CLI were rewritten around `mail api call <Method> '<json>'`, which is real and is the same call an agent makes over MCP. Review also caught: pages instructing the reader to type `:`-ranges on a **command line task 89 has not built** (rewritten, with a "What a colon spelling is" section in `manual` saying plainly that a verb's colon form is its *name*); a new model test that could not fail; two hand-written capability counts that were wrong (155, not 159); `daemon`/`practice-tokens` stating the peer-uid admin grant unconditionally when `client_auth.require_for_local` (shipped on this branch) turns it off, and calling the socket's 0600 mode "the access control" when `rmaild::auth` calls it defence in depth; and a `cited_ids` helper whose `&'static str` laundering could silently drop a marker hard-wrapped across two source lines.
- **note (grep cost, measured rather than reasoned about):** task 103 left "measure `Location::Grep` once 104 has written the pages" as the open question, since it renders *every* page. At 45 pages, unoptimised, in the test container: 6.3 ms for a whole grep render, 0.13–0.25 ms for one page. The worst frame is the hit list rendered twice — cursor span plus draw — so ~13 ms of debug build and a fraction of that optimised. No cache: it would have to be invalidated on every `keys.toml` reload, which is the staleness the generated pages exist to rule out.

## 105. Leader map, key vocabulary and migration
- [x] status
- **depends-on:** 91, 95, 96, 97, 98, 99, 100
- **parallel-safe:** no
- **acceptance:**
  - `<space>` installed as a leader in Normal/Viewer/Visual with the default group map (`<space>a` ai, `<space>t` tag, `<space>r` rule, `<space>d` daemon, `<space>c` config/settings, `<space>s` search/saved, `<space>o` outbox/send, `<space>x` extract/attach, `<space>n` note, `<space>g` goto, `<space>w` webhook/hook, `<space>h` help) — every group label still derived (task 91), not hand-written.
  - `Key` extended with `Left`/`Right`/`Home`/`End`/`PageUp`/`PageDown`, including `named_key` spellings and the crossterm-to-`Key` conversion, which silently drops them today.
  - `Keymap::shadowed_across_layers` (task 91) runs as a startup lint printing a warning for any hit, and is reachable as `:keys check`.
  - No default binding already shipped is removed or rebound; `palette` remains a working alias of `command`; a migration note in the manual (task 104) covers anyone whose own `keys.toml` already binds `:` or `<space>`.
- **verify:** `cargo nextest run -p rmail-core keymap::` · `cargo nextest run -p rmail-cli tui::model` (leader chords resolve to the right groups, new `Key` variants round-trip through parse/display, startup shadow-lint fires, no regression in existing default bindings)
- **verify:** `cargo deny check` · `cargo audit` · `buf breaking --against proto/buf-baseline.binpb` (this repo has no `main` branch or remote for `.git#branch=main` to resolve against — see `scripts/update-buf-baseline.sh`) · `cargo bench -p rmail-core --no-run`
- **note (a leader group needs members, and members need argument-free verbs):** a chord resolves to an `Action`, so a domain with no action has no group. Fourteen were added, one per verb that takes no arguments and acts on what is on screen — `tag list`, `tag suggest`, `rule list`, `rule run`, `sync status`, `index status`, `ai status`, `attach list`, `links`, `note list`, `note watch`, `webhook list`, `hook list`, `saved list`, plus `keys check`. Each runs its own verb through `run_verb`, so a key and the typed line are one code path and a key cannot do anything a line could not. A verb needing *words* — an address, a query, a tag name — has nothing a keystroke could supply and is deliberately absent from the map; task 101's settings screen is where those get put on the `:` line for you.
- **closed at merge — the dispatcher was routing on a proxy, and both proxies were wrong:** `run_invocation` sent a verb to the answer table when it had *no action*, which stopped being true the moment thirteen table verbs gained one (the verb is the capability's surface; the action is only the key that reaches it) — that routing sent `:tag list` to `run_action`, whose arm runs `:tag list`, forever. Switching to "has a capability" broke the other way: `Action::Delete`'s auto-derived `:message delete` carries `MailDelete` through `Capability::for_action`, and the table has no arm for it, so `:message delete` stopped asking before deleting. The condition now *asks the table* — `commands::answer` is pure, so asking twice cannot drift, and it is the only thing that actually knows.
- **note (the derived labels are honest about mixed groups):** `<space>a` reads `ai…` because its three members share a leading id segment. `<space>d` spans `sync.status`, `index.status` and `ai.status` and reads `3 commands`, because `common_id_prefix` refuses to invent a name — which is task 91's whole point and the reason there is no group table anywhere. The test derives each label from whatever is bound under the letter rather than restating it, so a member moved elsewhere changes the label rather than breaking the test.
- **note (bound once, live in three modes):** `Viewer` and `Visual` both chain through `Normal`, so every leader chord is live in all three from one binding. Three copies would be three things to keep in step in a user's `keys.toml` — and a test asserts the chord fires in all three *and* not from a modal layer, which is what the chain stopping at `Global` is for.
- **closed at merge — the map shipped one key that did nothing where it was bound:** `<space>sx` reached `search.explain`, which toggles the why-panel over a *result*; from the message list there is no result and the key was inert. The test that walks the whole map and refuses a member that changes neither the model nor the status line caught it. It stays on `x` in `Mode::Menu`, where it belongs.
- **note (the six keys were unwritable, not merely unbound):** `<left>`, `<right>`, `<home>`, `<end>`, `<pageup>` and `<pagedown>` were dropped by the crossterm conversion *and* unnamed by the chord grammar — so a binding on `<home>` could not be written down, let alone fire. Both halves are fixed, `<pgup>`/`<pgdown>`/`<pgdn>` are accepted as aliases (they display as one spelling, because a file this client rewrites has to be consistent with itself), and `tui::tests::every_named_key_survives_the_trip_from_the_terminal` pairs each `KeyCode` with the `Key` it must produce — the pairing being the point, since a key the keymap can *name* and the reader drops is worse than one neither knows about.
- **note (the lint runs on every load, so it has no startup path of its own):** `Msg::Keymap` is where it goes, which is the same reasoning `tui`'s own docs give for having no "load the keymap" step — the first load and every later edit take one path. The warning goes to the status line rather than stdout, because this process's stdout *is* the alternate screen and a `println!` would be written into cells ratatui does not know it wrote. `:keys check` is the detail the line points at, and it is asserted to be empty on a clean install — a shipped keymap that tripped its own warning would make the warning meaningless.
- **note (nothing shipped was removed or rebound):** the `DEFAULTS` diff is 28 additions and zero deletions. `palette` is still `Action::PaletteOpen` and still resolves, which is asserted rather than assumed. And `no_default_binding_can_ever_be_shadowed_in_its_own_layer` is the guard that matters going forward: the built-ins are installed through `insert`, so the check `bind` performs for a user's file does not run on them, and a single-key binding added later on `<space>` would silently bury all twenty-eight.
- **closed at merge — nine tests were pinned to `<space>` being free:** task 91's continuation fixtures used `<space>a`, `<space>q`, `<space>h`, `<space>s`, `<space>x`, `<space>ab` and `<space>b` precisely because nothing was bound there. They moved to `z`, which is bound in no layer. `keymap::file::tests`' quoting round-trip used a bare `<space>` as its "needs quoting" example and now uses `<space>z` — plus `<home>`, `<pagedown>` and `<left><right>`, which is a real round-trip test for the new spellings.
- **note (the supply-chain gate, and the one tool this host does not have):** `cargo deny check` reports advisories/bans/licenses/sources ok; `cargo audit` finds nothing across 586 dependencies; `cargo bench -p rmail-core --no-run` builds both harnesses. `buf` is not installed on this machine, so `buf breaking` was not run — and it cannot have regressed: `proto/` is byte-identical to what was there before this task (`git diff -- proto/` is empty), so there is no schema change for it to be breaking against.

## 106. Paging everywhere, and the manual's defaults
- [x] status
- **depends-on:** 84, 103, 104
- **parallel-safe:** no
- **acceptance:**
  - Two actions, `cursor.page-down` and `cursor.page-up`, bound to `<c-d>`/`<c-u>` in every layer that has something to page — `Normal` (so `Viewer` and `Visual` inherit them), `Prompt`, `Menu`, `Pick` and `Help` — and in neither `Insert` nor `Confirm`, where there are no rows and no scroll offset to move. One screenful less a row of overlap; a count means pages, the way vim's own page keys read one. No binding already shipped is removed or rebound, which keeps task 105's last clause true.
  - The model learns the terminal's height as a message (`Msg::Resize`), sent once at startup from `terminal.size()` and again on every crossterm resize, so `update` stays pure and `view` keeps its monopoly on layout. `Model::viewport_rows` is the only fact about the window the model holds, and `page_rows` is the only thing that reads it.
  - A worked example covering every kind of account and every credential source (`add-any-account`), and a `provider-settings` reference page carrying, per provider, the two hostnames, the two ports, whether a password is accepted at all, and where the app password or the OAuth client registration is found.
  - Every authored page states the defaults of what it describes, or names where they are read from — each value taken from `config::*::default()`, `.env.example` or the binary's own `--help` rather than restated from memory.
- **verify:** `cargo nextest run -p rmail-core keymap::` · `cargo nextest run -p rmail-cli tui::` (paging in each layer and each cursor, the resize message, and task 103's whole reconciliation suite against the two new pages)
- **note (the model had to learn one fact about the window, and only one):** `cursor.page-down` cannot be answered from the mailbox — a page is a property of the terminal — so `Msg::Resize` joins the message set and `Model::viewport_rows` is the one geometry field the model holds. `view` keeps its monopoly on layout: `page_rows` subtracts a fixed `CHROME_ROWS` of 3 (the status row and the pane's two borders) rather than the real chrome, which varies with what is on screen. Understating it makes a page slightly *taller* than the visible rows on a frame carrying a toast or the WhichKey band, which costs at most the row of overlap; deriving the real number would mean duplicating `view`'s layout inside the model, which is the coupling this design exists to prevent. `to_msg` was factored out of the input thread so the event→message mapping is testable without a tty.
- **note (what the reviewers caught in the account pages, which was most of their content):** three adversarial lenses over the two new pages returned 44 findings, and the code-truth lens was right about every claim it disputed. The load-bearing ones: `--ai` proposals are **never** login-verified (`Autoconfigurator::verify` refuses before resolving the credential — presenting a password to a host a model produced from attacker-controlled probe responses is not what consent to *ask* the model buys), and the refusal it prints tells the reader to run `mail account test`, which is not a verb; `AccountTestConnection` cannot check an OAuth account at all, because `imap::test_connection` resolves a password unconditionally and `CredentialSource::OAuth::resolve` refuses — so the page's "universal check" failed for exactly the two providers it had just told you take OAuth; probing stops at the first *parseable* candidate and a validation failure then aborts the discovery rather than falling through to the next source; `mail account refresh <id>` does not force anything without `--force`; a keychain lookup prompts too, while a `password_command` cannot (it runs with stdin and stderr null under a ten-second kill, so a tty pinentry fails rather than asking); and Google still accepts an app password on a personal account with 2-Step Verification, so "Google and Microsoft take OAuth and nothing else" was wrong about Google and contradicted this branch's own provider table.
- **note (a product bug the pages had to be written around):** an `[[accounts]]` block's connection keys — `imap_server`, `port`, `username`, `password_command`, `password_env`, `keychain`, `smtp_server`, `smtp_port` — are parsed by the schema and read by **nothing**. The only consumers of `config.accounts` in the workspace are `NotifyPolicy::from_config` and the AI policy engine, both of which read `name`, `ai` and `notify`. So `mail account add`'s printed block says "paste this into rmail.toml to apply it" about six keys that apply nothing, and a credential put there configures no login: the row `AccountService.Create` writes is what carries all of it. `config-file`, `add-any-account` and `troubleshooting` now say so. The fix belongs in the daemon rather than in prose — either reconcile the block into the row at startup, or stop the schema from accepting keys it ignores — and `AccountService` has no `Update`, so a wrong host or username today means Delete and Create again.
- **note (eleven pages' numbers were checked by hand):** the sweep ran a writer and an independent verifier per page; eleven verifiers died on a spend limit (`practice-index`, `practice-sending`, `practice-keymap`, `practice-accounts`, `practice-tokens`, `practice-rules`, `practice-notifications`, `practice-attachments`, `keys-toml`, `config-file`, `troubleshooting`). Every default those pages state was re-checked against the source instead: the notify table (off, `high`, `auto`, quiet hours off, 60m age bound), the send table (10s window, 5 retries, 30s doubling to 30m, 10m late tolerance, preflight on with `block_at = "block"` and 15 recipients), the rules table (5s tick, 200 batch, 8 examples, 30 dry-run days), `index.extract` (25 MB, OCR off, `["eng"]`), `ai.ask` (`claude-sonnet-5`, top_k 12, 8000/2000/1024), `MAX_KEYS_BYTES` 256 KB, `mail daemon start --timeout` 30s against the 5s socket probe, and `token create`'s required `--scope` with its eight-scope vocabulary. Two host scripts kept the mechanical half honest between container runs: one reproduces the manual's build invariants (markers resolve, fences balance, no unwrappable token, `documents` claims cited, every page linked) and one runs each fenced `mail …` line through the real clap tree.
- **note (the brief was wrong before it was right):** the sweep's reference lists were extracted from the *shared checkout*, which another session was mid-task-95 in, so they carried `tag rules`, `tag rules set` and `rule backtest` — verbs that do not exist at this commit. Three pages cited them as `{{cmd:…}}` before the host checker caught it; they now name the capability or write the `mail` line as prose. A brief derived from a working tree is a brief that documents someone else's uncommitted work.

---

# PART VI — TUI v2 "Cockpit" (ground-up redesign per `tui.md`)

Decomposed from `tui.md` ("rmail TUI — Design Specification", codename Cockpit; committed
as `5f79822`). `tui.md` is this part's PRD — every acceptance below cites the section it
implements (`§N`) so drift is checkable by re-reading one paragraph, not the whole
document. **Read `tui.md` in full before touching any task in this part**; it is 1539
lines and every clause is load-bearing.

**What this part is, precisely (`tui.md`'s own framing, §0/§21):** a redesign of *what the
view draws and how interaction feels*, not of the architecture underneath it. Part V's
invariants carry over **verbatim** and are re-asserted here as a standing constraint on
every task, not repeated per-task:

- Elm-style model: pure, synchronous, clockless `update(&mut Model, Msg) -> Vec<Cmd>`;
  `Model::mode()` derived, never stored.
- `view.rs` stays the **one** ratatui-aware module; no other module imports ratatui.
- One vocabulary: every bindable `Action` is a parity capability or `LOCAL_ACTIONS`; keys
  and `:` lines share `run_verb`; the CI drift check (`every_tui_action_is_a_capability_or_declared_local`)
  stays green through every task below.
- Generated discoverability: help/which-key/keybar/manual footers derive from the live
  `Keymap` + verb registry; drift is a failing test, not a docs task.
- `terminal_safe` sanitizes every untrusted string before it reaches a `Line`/`Span`.
- Stream discipline: every stream generation-stamped; supersession aborts client-side;
  daemon `CANCELLED` on a superseded stream is silence.
- Thin client: gRPC only, no SQLite/IMAP/filesystem state beyond `keys.toml`/`tui.toml`/
  history; attach < 200 ms, first frame paints before any RPC returns.
- `keys.toml` hot-reload, shadow lint, chord grammar, history ring + secret filter, and the
  `Report`/`Form`/`ConfigBlock` engines all keep their current call contracts (this part
  relocates *where* they render — inside cards/overlays — never *how they're driven*).

**No backend/proto work in this part.** Every RPC `tui.md` §17's wiring map names was
verified against `proto/*/v1/*.proto` before writing this breakdown — all of it already
exists and already has a v1 CLI caller (tasks 1–82). `tui.md` §19's thirteen daemon gaps
are honest UI-side degradations by design (labeled, never faked) — task 175 wires the
labels; **no task in this part adds an RPC**. If an acceptance below turns out to need
one, stop and amend `tui.md`/this file rather than inventing a client-side workaround
that papers over it (law 6, §1.1).

**Cross-cutting acceptance, implicit in every task below** (the reviewer checks these on
every diff in this part; restating them 73 times would just make them easier to skim
past): the ten laws of §1.1 — spatial stability, one meaning per key (deviations only via
the §8.3 arbitration table), the Esc ladder is the *only* place Esc is handled, overlays
add keys and never rebind one, no invisible state, honest counts (`~`, `•`, `(partial)`),
optimistic+undoable over confirmed, never block/never blank a frame, model text in `ai`
tint **and** `«»` guillemets, and the daemon decodes/the client only renders. A task whose
diff violates one of these is not done regardless of what its own acceptance says.

**Supersedes, task by task (`tui.md` §21 "Replaced"/"Deleted concepts"):** the three-screen
`Screen` enum → frame + collections + full-frame apps (dismantled across 119, 150–156);
single `Option<Overlay>` → stack of ≤3 (108); headers-only preview → full Reader card with
body (126–127); the bespoke AI panel column → rail tab `✦` (128); `O` outbox overlay →
outbox collection (157). Part V's tasks stay checked — they shipped and were correct for
the design they targeted — but by the time Part VI's task 179 closes, none of the
superseded rendering paths should still be reachable from a live keybinding.

**Sizing note:** these are larger than most Part I–V tasks — `tui.md` itself says so
(§1.3's defect table, §4.2's arithmetic proofs, §18's binding checklist are each already
implementor-grade detail this file does not need to re-derive). A task below is still
independently shippable and independently testable; "a few hours" becomes "up to a day"
for the handful of genuinely architectural ones (107, 119, 132), which is why those are
sequenced first and everything else depends on them transitively.

## 107. Card/deck router — `layout_mode` and `DeckPlan`
- [x] status
- **depends-on:** none
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/layout.rs`: `fn layout_mode(area: Rect) -> DeckPlan`, the single
    source of truth for which of the four cards (Sidebar/List/Reader/Rail) are visible,
    their `Rect`s, drawer placement, and focus-ring order — §2.2.1, §4.2, §4.3.
  - Implements the five width breakpoints (XS<80, S 80–119, M 120–159, L 160–199, XL≥200)
    and their exact constraints table (§4.2) and the five height tiers with their documented
    drop orders (§4.3) — nothing silently vanishes; `DeckPlan` exposes what a bar shed at
    this size so a caller can render the one-keypress-away hint.
  - The L-floor arithmetic in §4.2 (`160−5−22−34=99`, `Fill(5)/Fill(5)`→List≈49/Reader≈50)
    holds as a literal unit test, not just a comment.
  - No two cards' `Rect`s ever overlap and no `Rect` ever exceeds `area` at any size in the
    tested range — the Adaptive-design "126>120" defect class (§4.2) cannot recur silently.
  - Nothing in this task renders anything yet — `view.rs` is not touched; this is the pure
    function later card tasks (120–131) consume. `DeckPlan` deliberately has no `Style`/
    `Color` field (layout is not paint).
- **verify:** `cargo nextest run -p rmail-cli tui::layout` (breakpoint table, height-tier
  drop order, the L-floor arithmetic assertion, and a property test sweeping every width
  20..400 and height 10..100 asserting no overlap/no overflow — the exact matrix §18 craft
  rule 2 and Appendix A both call for)
- **note (host toolchain drift, fixed as a prerequisite):** this host's Rust (1.98.0) has newer default-warn clippy lints than whatever this repo was last gated against — `cargo clippy --all-targets --all-features -- -D warnings` failed on a clean `main` checkout with zero of this task's changes applied (verified via `git stash` before writing any code). Fixed with scoped `#[allow]`s matching existing precedent (`rmail-proto/src/lib.rs`'s pre-existing `large_enum_variant` allow, `rmaild/tests/client_auth_service.rs`'s pre-existing `result_large_err` allow) plus one dead `use futures::StreamExt as _;` removed in `rmail-core/src/compose/reply/tests.rs`. None of this touches `tui/`; it unblocks the gate for every task in this file, not just this one. `rmail-cli/src/tui/layout.rs`'s own doc comment on its `#[allow(dead_code)]` explains the separate (expected) reason the whole new module trips that lint until task 120 wires it in.
- **closed at merge — the reviewer found two real bugs in the first draft:** `split_with_separators` computed a 1-column gap between cards for the `Borders::LEFT` seam but then discarded it (`.step_by(2)`), so the seam belonged to no card's `Rect` — task 120 applying `Borders::LEFT` per §4.1 would have silently eaten an extra column from whichever card drew it, undercutting §4.2's declared budgets exactly the way the "126>120" defect class this task's own acceptance forbids. Fixed by merging each seam into the *following* card's `Rect` as its own leftmost column, so a `Borders::LEFT` block drawn on the `Rect` as returned needs no separate "which cards need an extra column" bookkeeping. Second: `HeightTier::Bare` (<15 rows) was never actually collapsed to a single card — only `Minimal` triggered the S-breakpoint's slide-between — so a sub-15-row terminal at M/L/XL rendered all four cards, and at S specifically produced a `Placement::Shown` Reader with `height: 0` (reported visible, renders nothing — the exact "silently vanishes" law 6 forbids). Fixed by resolving `HeightTier::Bare` before the breakpoint dispatch in `layout_mode`, sharing the existing XS single-card logic (factored into `single_card`) rather than duplicating it. Both were caught because the reviewer extracted the module into a scratch crate and swept it empirically rather than trusting the property test's own coverage — which had a third, related gap: its fixtures hardcoded `height_tier` independently of the swept height, so `Bare` was never actually exercised by the sweep that was supposed to catch exactly this. Rewritten to derive `height_tier` from the swept height every iteration, and to assert non-degenerate size on every visible placement (not just containment), which is what would have caught the zero-height Reader directly.
- **note (property test narrowed to boundary-focused sampling, not exhaustive):** the first draft swept literally every width 20..400 and height 10..100 (measured: 109.6s, the single slowest test in the entire workspace by two orders of magnitude against a ~49-69s full-suite baseline). `layout_mode` is piecewise-linear within a breakpoint/tier band — no branch on the raw width/height value once the breakpoint is chosen — so a defect that held at both ends of a band and broke strictly inside it is not a bug class this arithmetic can produce; verified by keeping the exhaustive version's pass/fail identical to the boundary-focused version's before cutting it. Narrowed to every breakpoint/tier boundary ± 1 plus interior samples (`WIDTHS`/`HEIGHTS` consts in `layout/tests.rs`): 0.6s, same coverage where it matters.

## 108. Overlay stack (replaces the single-overlay slot)
- [x] status
- **depends-on:** none
- **parallel-safe:** no
- **acceptance:**
  - `Model`'s `Option<Overlay>` becomes `overlay_stack: Vec<Overlay>` capped at depth 3
    (§2.2.2, §3.1) — `push_overlay` refuses a fourth with a status-line explanation rather
    than silently dropping the oldest (which would close something the user still has open).
  - Every existing overlay call site (finder, command line, pickers, attachment browser,
    confirm, help, quick menu, image viewer) migrates to push/pop against the stack;
    behavior is preserved for the single-overlay case (today's tests keep passing).
  - `Esc` pops exactly the innermost overlay (stack step 2 of §4.6 — the full ladder is
    task 115; this task only guarantees the stack itself pops one, LIFO, correctly).
  - Render order is back-to-front by stack index; only the topmost overlay receives key
    input (lower ones are visually present — e.g. confirm-over-picker — but inert).
  - The which-key band is explicitly **not** an overlay (§3.1) — confirmed by a test that
    a pending chord and an open overlay can coexist without the band occupying a stack slot.
- **verify:** `cargo nextest run -p rmail-cli tui::overlays` (push/pop/cap-at-3/refusal
  message, z-order rendering, input routed only to the top, which-key band excluded)
- **note (verify line corrected):** the acceptance's z-order rendering proof lives in `tui::view::tests` (only `view.rs` calls `render`, per this codebase's "one ratatui-aware module" invariant — `tui::overlays` deliberately holds no rendering code), and the confirm-carries-a-report regression this task's own review surfaced (see "closed at merge" below) landed in `tui::report::tests`, where the confirm-over-report machinery already lived. Full verify: `cargo nextest run -p rmail-cli tui::overlays::` (push/pop/cap-3/refusal/LIFO/which-key-coexistence — 9 tests) · `cargo nextest run -p rmail-cli tui::view::tests::a_stacked_overlay|tui::view::tests::overlay_stack_render_order` (z-order — 2 tests) · `cargo nextest run -p rmail-cli tui::report::tests::esc_on_a_confirm_restores` (the Esc-path regression — 1 test).
- **note (the scope line the acceptance draws, and why):** every one of the ~20 pre-existing "open an overlay" call sites was migrated to a new `Model::set_overlay` (replace-the-top-or-push-if-empty), not to `Model::push_overlay` (genuine stacking) — reproducing `Option::= Some(x)`'s old behavior exactly rather than silently starting to stack. `push_overlay` itself is proven only by its own new tests in `tui::overlays::tests`; nothing in the existing call graph reaches it, so it and `MAX_OVERLAY_DEPTH` carry `#[allow(dead_code)]` in the non-test binary target — the same "declared shape a future task consumes" pattern task 92 established for `Toast::Completion`/`Toast::Priority`. Reviewed and endorsed as the right scope for the *open* sites; see the next note for where the same "always replace" instinct was wrong.
- **closed at merge — the reviewer found a real bug in `leave()`'s carried-report restore:** the "a question asked over a report puts the report back" step (declining/answering a confirm that carries `Confirmed::Invoke { over: Some(report), .. }`) used `set_overlay` to restore it — correct only by accident, because every reachable state today is at most one overlay deep. `set_overlay` replaces whatever is *now* on top; had this confirm been pushed (via a future genuine `push_overlay` call site) over a third layer per tui.md §2.2.2's own "confirm over picker over collection", popping the confirm and then calling `set_overlay` on the way out would have silently clobbered that third layer with the restored report instead of leaving it where popping the confirm already put it. Fixed with `Model::restore_overlay` (push back exactly what was popped), which is the primitive this exact "pop, inspect, put back" idiom already existed for elsewhere in the same function. Caught because the review insisted the regression test drive the real `Esc` key through `leave()` rather than calling `pop_overlay` directly (a first draft did the latter and could not have caught its own bug); verified by reverting the fix and confirming `tui::report::tests::esc_on_a_confirm_restores_the_report_underneath_without_clobbering_a_deeper_layer` fails exactly as predicted (depth 1 instead of 2) before re-applying it.
- **note (ten more call sites needed a third primitive, `clear_overlays`):** six sites doing `model.overlay = None` and four more doing `.take()`-then-cancel-its-stream all predate this task and each already carries a doc comment stating the invariant as "no overlay may be left open" (e.g. `open_manual_at`: "the manual is a screen, so an overlay left up would cover the thing the caller just asked to show") — never "one fewer overlay." Migrating them to `pop_overlay` (which only reaches the top) would have silently regressed that invariant the moment any stack ever got two deep. Added `Model::clear_overlays` (drains the whole stack, returns what was cleared so the four stream-owning sites can `.iter().flat_map(cancels)` across all of them instead of only the topmost) and moved all ten sites onto it. Also made `overlay_stack` a private field (was `pub`, which made the depth cap purely advisory — any code, including a future call site, could `push` straight past it) behind a new read-only `Model::overlays() -> &[Overlay]`; every write path was already exclusively the five methods this task added, so no behavior changed, only what could bypass them.

## 109. Zoom + drawer state
- [x] status
- **depends-on:** 107
- **parallel-safe:** no
- **acceptance:**
  - `Model` gains `zoom: Option<Card>` (per-card sticky: survives focus changes and
    resizes) and drawer state derived from `(focus, layout_mode(area))` rather than stored
    — "focus leads, layout follows" (§4.4): focusing a card the breakpoint hides summons it
    as an ephemeral drawer (sidebar left `Length(24)`, rail right `Length(34)`, full-frame
    at XS); moving focus away closes it with no separate "open overlay" key.
  - `Z` toggles zoom on the focused card, full-bleed inside the card area (§4.5). Zoomed
    List renders as the headed sortable triage table (implemented fully in 143; this task
    only wires the zoom *state* and a placeholder full-bleed render so the toggle is
    observable and tested before 143 lands).
  - `\` (rail) and `C-b` (sidebar) flip *default visibility* at breakpoints that can afford
    the card, and focus-summon the drawer at narrower ones — same key, same meaning, §4.4's
    "there are no separate open-sidebar-overlay keys" is a literal test (grepping for a
    second binding would fail it).
  - Zoomed Sidebar is permitted (rule-consistency, §4.5) even though it has no special
    render — a test asserts `Z` on the focused sidebar doesn't panic or dead-end focus.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (zoom toggle per card persists
  across a resize and a focus change, drawer summon/dismiss at each breakpoint via
  `layout_mode`, `\`/`C-b` dual behavior at wide vs. narrow widths)
- **closed at merge — the reviewer found the same state-machine hole in both halves of the
  sidebar/rail toggle, one round apart:** `toggle_sidebar`/`toggle_rail`'s narrow-width branch
  (focus-summoning a drawer) left `model.zoom` untouched when the zoom was on a *different*
  card, so summoning e.g. the sidebar with `C-b` while the reader was zoomed left the zoomed
  reader on screen while focus silently moved to a sidebar nothing was rendering — round 1
  caught this and it was fixed by clearing `model.zoom` in that branch. Round 2 found the
  *affording*-width branch (the preference flip at ≥120 columns) carried an equivalent bug: it
  flipped `sidebar_visible`/`rail_visible` without touching an active zoom, plus emitted a
  status message claiming "sidebar on" even when zoom meant nothing changed on screen. Here the
  fix diverged from the reviewer's own first suggestion: the affording branch sets a
  *preference* for the eventual renderer to consult on a later frame, not an immediate visual
  promise the way narrow-width focus-summon is, so clearing zoom there would break the
  pre-existing (task 107) "zoom always wins" invariant for no reason — the reviewer's second
  pass retracted its own suggestion and agreed. `affords_split`'s two branches now carry doc
  comments spelling out which is a promise and which is a preference, and why only one clears
  zoom.
- **closed at merge — `Z` had no observable effect and was reachable from the wrong screen:**
  the first draft toggled `Model::zoom` with no corresponding render, so the acceptance's own
  "the toggle is observable and tested" requirement was unmet outside unit assertions on the
  field itself — fixed by `render_zoomed_placeholder` in `view.rs`, a full-bleed block naming
  the zoomed card, gated on `Screen::List`. Round 2 caught that the gate was missing at the
  `update` layer too: `Mode::Viewer`'s keymap chain inherits `Mode::Normal`
  (`keymap/mod.rs:539`), so `Z` was live while a message was open in the old `Screen::Viewer`
  even though nothing there ever renders a zoom placeholder — pressing it silently changed
  `model.zoom` with zero visible effect and no way back short of guessing. `toggle_zoom` now
  refuses outside `Screen::List` with a status-line explanation instead.
- **note (round 3, a loose assertion that could pass for the wrong reason):** two
  placeholder-naming tests (`a_zoomed_card_replaces_the_panes_with_a_named_placeholder`,
  `the_zoomed_placeholder_names_whichever_card_was_just_zoomed`) asserted only that the
  rendered frame contained a loose substring like `"list"` or `"reader"`, which the status
  line's own `"list zoomed"`/`"reader zoomed"` message could satisfy independently of whether
  `render_zoomed_placeholder`'s title was ever drawn. Tightened to the placeholder's actual
  em-dash title form (`"list — zoomed"`), which only the placeholder itself can produce.
- **note (manual page and task 132's undecided rule):** the manual-coverage gate required a
  documenting page for `card.zoom`/`sidebar.toggle`/`rail.toggle` before this task's own diff
  could land; added `cards-and-zoom.md`, careful to describe only what's actually observable
  today (state changes plus the zoom placeholder), not the four-card deck this task's state is
  built for but doesn't render yet. Also added an acceptance bullet to task 132 (the future
  zoom/focus rule) pinning the one interaction this task's own tests establish: moving focus
  onto a *hidden* card clears zoom, moving between two already-*visible* cards does not — §4.5's
  "survives focus changes" language covers only the second case, which task 132 will need to
  know explicitly rather than re-deriving.

## 110. Filter engine (client-side predicate grammar subset)
- [ ] status
- **depends-on:** none
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/filter.rs`: parses and evaluates, over **loaded rows only**, the
    client-safe subset of the search operator grammar — `from: to: subject: is: has: tag:
    ai:` plus free text (§10) — with zero RPCs and sub-frame latency.
  - Any operator outside the safe subset (`before:`, `after:`, `on:`, `date:`, `note:`,
    `~`, `=`, NL text, anything server-only) is **rejected inline**: the parser returns a
    typed `Unsupported(operator)` rather than silently ignoring it or erroring generically,
    so the caller can render it red with "use `/` for that" (§10) — wiring that rendering
    into the actual `f` prompt is task 141; this task proves the parser's classification is
    exhaustive over every operator §9.2 lists.
  - Predicate evaluation is pure `(FilterExpr, &Row) -> bool`, unit-tested against the
    fixture rows already used by list tests — no new RPC, no new `Cmd`.
- **verify:** `cargo nextest run -p rmail-cli tui::filter` (every safe operator matches/
  excludes correctly, every unsafe operator classifies as `Unsupported` by name, negation
  and free-text-over-loaded-fields both work, empty filter is a no-op identity)

## 111. Client unread ledger
- [ ] status
- **depends-on:** none
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/ledger.rs`: per-folder unread estimates maintained from loaded
    rows on first load, then adjusted incrementally from `WatchEvents` deltas (a message
    arriving unread increments, a `\Seen` flag change decrements) — §2.2.4.
  - Every derived count renders with the `~` prefix (law 6); a folder never visited this
    session renders `•`, never a bare number — the ledger's public read API returns an enum
    (`Unknown`, `Estimated(u64)`) precisely so a call site cannot accidentally print a raw
    integer and lie about precision.
  - A folder swept clean by a bulk action (mark-all-read) or one that receives a burst of
    new mail while unfocused both converge to the correct estimate without a full re-list —
    proven by a test that replays an event sequence and checks the running count at each step,
    not just the final one.
- **verify:** `cargo nextest run -p rmail-cli tui::ledger` (initial estimate from a loaded
  page, increment/decrement from synthetic `WatchEvents` deltas, `Unknown` vs `Estimated`
  rendering contract, convergence under an interleaved event replay)

## 112. Undo stack (session-local inverse operations)
- [ ] status
- **depends-on:** none
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/undo.rs`: a session-local stack of inverse operations
    (move-back, unflag, untag, restore-from-trash, `CancelScheduled` within its window),
    each carrying the idempotency key its forward action used so a retried undo cannot
    double-apply (§2.2.8).
  - **No redo** — the stack has no forward-replay method at all, not merely an unbound one;
    §2.2.8's stated reason (inverse-op redo over a drifting IMAP mailbox lies) is enforced
    by the type not existing, so a future task cannot casually add one without deleting this
    acceptance first.
  - `u` pops and issues the inverse `Cmd`; an empty stack renders "nothing to undo" in the
    message zone rather than being silently inert (honesty over polish, law 6/9 of §1.1 —
    a keypress that does nothing and says nothing is indistinguishable from a dropped key).
  - The undo-send special case (chip, not a toast, driven by `undo_deadline`) is **not**
    built here — that's task 146, which reuses this stack's `CancelScheduled` entry.
- **verify:** `cargo nextest run -p rmail-cli tui::undo` (push/pop ordering, idempotency
  key carried through to the reissued `Cmd`, empty-stack message, no public redo method —
  a compile-fail test via `trybuild` or a doc-comment-checked absence is acceptable)

## 113. Lens engine (pinned queries, honest counts)
- [ ] status
- **depends-on:** 111
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/lens.rs`: a `Lens` (name, compiled query, auto-assigned stable
    mnemonic letter) list seeded with the four built-ins (`is:unread`, `ai:needs-reply`,
    `ai:category:newsletter`, `ai:category:receipt`) plus user-pinned ones (§5.2).
  - Count honesty machine implemented exactly as §5.2 specifies: Unread lens count comes
    from the ledger (111) rendered `~9`; any lens visited this session keeps its last result
    count, trailing `·` if `WatchEvents` dirtied its scope since; never-visited renders `•`;
    **no background counting by default** — a `lenses.count_refresh` field
    (`"manual"|"on-idle"`) gates the one optional bounded refresh sweep, default `"manual"`.
  - `''` flips to the last-visited lens (a one-slot history, not the full stack); `<`/`>`
    cycle; mnemonic letters are stable across a session (re-deriving them on every render
    would make `'r` sometimes point at a different lens than the keybar just showed).
- **verify:** `cargo nextest run -p rmail-cli tui::lens` (mnemonic stability across
  re-renders, the full count-honesty state machine — unvisited/visited/stale/refreshing —
  `''` flip, and that `count_refresh="manual"` issues zero RPCs on an idle tick)

## 114. `tui.toml` local prefs store
- [ ] status
- **depends-on:** none
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/prefs.rs`: a serde struct covering exactly §2.2.6's field list —
    theme, icon tier, density per collection-kind, sort per collection-kind, rail
    visibility + active tab, sidebar visibility, panel width overrides, hints on/off, mouse
    capture, undo-advance direction — with documented defaults for every field (no field is
    ever genuinely absent; unset means "use the shipped default", never "unknown").
  - Resolves `Model::rail_visible`'s width-dependent default: task 109 hardcoded it `false`
    at construction rather than consulting `layout::default_rail_visible` (`>= 176`), by
    this task's own design (there was nowhere to persist an override against it yet, and no
    live path to keep it in step as the terminal resizes). This task both computes the
    documented default from the *current* width when no stored preference exists, and
    decides — and tests — what a live `Msg::Resize` crossing 176 does to a value the user
    has never explicitly toggled versus one they have.
  - Write-through, debounced 1 s (matches `keys.toml`'s existing hot-reload cadence in
    spirit, but this direction is writes, not reads) — a burst of ten preference changes in
    one second produces one disk write, proven with a fake clock rather than a real sleep.
  - Malformed or partially-written `tui.toml` (a crash mid-write, a hand-edit typo) falls
    back to defaults for the unparseable fields only, not the whole file — one bad key must
    not lose every other preference — and is reported once in the notification feed, never
    silently.
  - File path follows the same XDG convention as `keys.toml`/history (co-located, not a new
    top-level dotfile).
- **verify:** `cargo nextest run -p rmail-cli tui::prefs` (round-trip every field including
  each collection-kind's per-kind sort/density map, debounce coalesces a burst to one
  write via a fake clock, partial-corruption recovery keeps the unaffected fields, missing
  file produces documented defaults)

## 115. The Esc ladder (one rule, implemented once)
- [ ] status
- **depends-on:** 108, 109, 110, 112
- **parallel-safe:** no
- **acceptance:**
  - One function, `fn resolve_escape(model: &Model) -> EscStep`, implements the exact
    8-step precedence in §4.6 and is the **only** place `KeyCode::Esc` is matched anywhere
    in `tui/model.rs` — a grep-based test (`esc_is_handled_in_exactly_one_place`) fails the
    build if a second match arm on Esc appears anywhere in the module.
  - All eight steps are individually reachable and individually tested: pending chord/count
    clears without touching an overlay beneath it; innermost overlay closes without
    canceling a stream also in flight; an active stream (search/ask/analyze/find) cancels
    with the daemon `CANCELLED` handled as silence (stream discipline invariant), leaving
    the previous collection's kept rows in place; zoom clears before the ladder considers
    navigation; visual mode exits before marks clear (two steps, proven as two, not one);
    an active filter clears; a non-root breadcrumb pops one level; at root, nothing fires
    and the status hint names `q` / `Ctrl-C Ctrl-C`.
  - `q` is a **separate** binding implementing only steps 7–8 (pop; at root, quit with a
    1-line confirm if a send is in its undo window) — proven distinct from Esc by a test
    that puts the model in a state where step 3 (cancel a stream) would fire for Esc and
    asserts `q` does not touch the stream at all.
  - `Ctrl-C Ctrl-C` (double-tap within 1 s) quits from any state, unbindable — not reachable
    through `keys.toml` at all (a rebind attempt on it is refused by the shadow lint with a
    named reason). A single `Ctrl-C` behaves exactly as Esc (delegates to `resolve_escape`,
    does not duplicate its logic).
- **verify:** `cargo nextest run -p rmail-cli tui::model` (all eight steps individually,
  the single-match-site grep test, `q` vs. Esc divergence, double-`Ctrl-C` timing window
  including the boundary — 999 ms fires as Esc, 1001 ms after the first tap does not chain)

## 116. Wrapped-text cache
- [ ] status
- **depends-on:** none
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/wrap_cache.rs`: a cache keyed `(message_id, width, fold_state)`
    over `textwrap`-produced `Vec<Line>`, invalidated on resize and on a fold toggle (quote/
    signature/section) for the affected message only — §2.2.10, feeding §7.1/§7.4.
  - Bounded size (an LRU or generation-based eviction) — a session that opens a thousand
    messages does not grow this cache without limit; the bound and its eviction order are
    asserted by a test, not left to "probably fine".
  - Cache hit is O(1) and allocation-free on the read path (§18 rule 9, frame budget 4 ms)
    — proven by a test that reads the same key 10,000 times and asserts wall time is
    dominated by the first (miss) call, not the following hits.
- **verify:** `cargo nextest run -p rmail-cli tui::wrap_cache` (key includes all three
  dimensions independently — same message different width misses, same width different
  fold-state misses — resize invalidation, bounded eviction order, hit-path allocation
  count via a counting allocator or equivalent instrumentation)

## 117. Color system v2 — token ramp, contrast lint, quantization, themes
- [ ] status
- **depends-on:** none
- **parallel-safe:** no
- **acceptance:**
  - `theme.rs`'s `Theme` struct is extended to the full §13 token table — `fg`/`fg_muted`/
    `fg_faint`, `bg_selection`/`bg_select_blur`, `border`/`border_focus`(=`accent`),
    `match_hl`, `unread`/`to_me`/`flagged`(=`scheduled`), `pri_high`/`pri_crit`, `ok`/`warn`/
    `err`/`info`, `ai`, `entity`, `link`, `quote1..4`, `acct1..6`, `diff_add_bg`/
    `diff_del_bg` — every one a **named field**, never a bare literal at a call site (the
    existing "no `Color::` literal outside `theme.rs`" lint, extended to cover every new
    token).
  - The contrast lint asserts the floors §13 states — body ≥ 7:1, muted ≥ 4.5:1, faint ≥
    3:1 — computed by WCAG relative luminance against the *painted* `bg`, for **both**
    `bg` and `bg_selection` backgrounds, as a compile-time-adjacent test (fails CI, not a
    runtime check). The one documented exception is enforced as code, not a comment:
    `fg_faint` inside a selection/cursor bar promotes to `fg_muted` in the row renderer,
    with a test that specifically covers this promotion (§13's footnote).
  - Truecolor values match §13's hex table exactly; a 256-color quantization path exists
    and is hand-verified to keep `muted ≠ faint` post-quantization (a literal equality
    assertion between the two quantized indices would catch a future accidental collapse).
  - `dark`/`light`/`mono`/`high-contrast` all exist and satisfy the mono rule already
    proven for v1 (no state carried by hue alone — every colored state pairs with a glyph
    or `Modifier`) extended to every *new* token this task adds, not just the ones v1 had.
    `NO_COLOR`/`TERM=dumb` renders attributes only, matching §13's exact list (bold unread,
    reverse selection, underline links/focus, `«»` carries AI, glyphs carry the rest).
- **verify:** `cargo nextest run -p rmail-cli tui::theme` (full token table present, lint
  forbids stray `Color::` literals workspace-wide via a source-scan test, all four
  contrast floors on both backgrounds for all four themes, `fg_faint`-in-selection
  promotion, quantization keeps `muted≠faint`, `mono` and `NO_COLOR` carry every state by
  non-color means)

## 118. Icon tiers (`Icons` struct)
- [ ] status
- **depends-on:** 117
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/icons.rs`: the three-table `Icons` struct (unicode default /
    nerd opt-in / ascii fallback) covering every glyph §6.1's table names — the 1 mark
    cell + 4 glyph cells (unread, addressed/replied/forwarded, attachment/scheduled/note,
    AI-and-safety with its precedence order) — plus every other glyph the spec names
    elsewhere (`◷ ⧗ ⧉ ✦ ⚠ ▲ ‼` etc., collected once here rather than inlined per call site).
  - Tier selection is a `tui.toml` field (114) with `unicode` as the default and an
    explicit opt-in for `nerd`; `ascii` is the automatic fallback when the detected
    terminal can't be trusted for wide glyphs (mirrors the existing `NO_COLOR`/`TERM=dumb`
    detection pattern from theme.rs) — detection logic is unit-testable against injected
    `TERM`/env values, not real terminal probing.
  - Cell-4 (AI & safety) precedence order is enforced by construction — a function that
    takes the set of active flags for a row and returns exactly one glyph, ordered
    injection > critical > high > needs-reply > pending > artifact, tested with every
    pairwise combination, not just the full-set case.
  - Every glyph has a documented ASCII fallback (§6.1's table is exhaustive — `N`, `>`,
    `r`, `f`, `@`, `~`, `n`, `!`, `!`, `^`, `?`, `.`, `+`) — a test asserts no unicode glyph
    in the struct lacks an ascii counterpart.
- **verify:** `cargo nextest run -p rmail-cli tui::icons` (all three tiers populated for
  every named glyph, cell-4 precedence over all pairwise flag combinations, tier
  auto-detection from injected env, exhaustive ascii-fallback coverage)

## 119. Collection engine — the `Collection` trait and registry
- [ ] status
- **depends-on:** 107
- **parallel-safe:** yes
- **acceptance:**
  - New `rmail-cli/src/tui/collection/mod.rs`: a `Collection` trait object the List card
    renders polymorphically — §2.2.3, §3.3's k9s-model promise ("one table engine, many
    resources"). Trait surface covers exactly what §3.3/§5.4/§16 require of every
    implementer: declared columns per density, row verbs (mapped through the arbitration
    table, task 135), title chips, and the detail renderer the Reader card shows for its
    rows (§5.5's "for non-message collections the Reader renders that collection's detail").
  - Two reference implementations ship in this task to prove the trait is sufficient
    without over-fitting to one shape: `FolderCollection` (flat, paginated, the existing
    `Mail.List` data reshaped behind the trait) and `SearchCollection` (streamed, ranked,
    server-thread-collapsed) — chosen because they're the two most different existing data
    shapes (flat-paginated vs. streamed-ranked) and between them exercise every trait
    method at least once.
  - `gm`/`gu`/`go`/`gf`/`gw`/`gn`/`gj`/`gd`/`gv`/`gi`/`gr`/`gs`/`gh` (§3.2, §16) resolve
    through **one** registry lookup (collection-kind → constructor), not thirteen
    hand-written dispatch arms — proven by a test that every `g`-chord target in §8.2's
    goto table has a registry entry, so a future collection (157–174) is "add one registry
    row" rather than "add a match arm in N places".
  - `go` (outbox) and `gm` (mail) are proven to share every generic List/Reader verb (open,
    scroll, mark, sort-where-applicable) through the same code path — §3.3's literal claim
    ("`go` is exactly `gm` with a different collection loaded") is a test, not a comment.
  - This task does **not** wire the real outbox/follow-ups/etc. data (157+ do); its `go`
    registry entry may point at a minimal stub collection proving only that the registry
    mechanism works end-to-end.
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (trait object dispatch for
  both reference impls, registry completeness against every §8.2 `g`-chord, the `go`≡`gm`
  shared-code-path proof, column/row-verb/title-chip/detail-renderer contract satisfied by
  both impls)

## 120. Outer grid, header band, banner row
- [ ] status
- **depends-on:** 107, 114
- **parallel-safe:** no
- **acceptance:**
  - `view.rs`'s top-level layout switches to the exact §4.1 vertical stack via
    `Layout::vertical(...).areas()`: header(1) → banner(0|1, reserved as 1 whenever any
    banner-worthy condition exists this session so toggling never shifts rows) → lens
    strip/breadcrumb(1, dropped <25 rows) → cards/full-frame(Fill) → which-key(0..2) →
    status(1) → keybar(1, dropped <25 rows). Toasts float, no layout row (§4.1).
  - Header band renders exactly §5.1's line: identity (`rmail ▸ account ▸ location`,
    account segment tinted `acct1..6`; `∑ unified` in unified mode) — gauges (`SYNC`/`IDX`/
    `AI`/`OUT`/`NET`, each with its documented tone ladder `? ✓ ↻ ‖ · ! ✗`) — session tally
    + clock on the right. Fed by the existing 5 s heartbeat (task 92's `Cmd::Heartbeat`,
    extended to the gauges this line adds) which **never** increments `inflight`, per the
    invariant task 92 already established and this task must not regress.
  - Header narrowing drop order matches §4.3 exactly: verb labels → `NET` → `OUT` → `IDX`
    detail (dot stays) → session tally → clock — each step individually reachable by
    shrinking the test terminal one column at a time, not just the endpoints.
  - Banner row renders the offline/degradation states (populated fully by task 153; this
    task only proves the reserved-row mechanic: a banner going from absent→present or
    present→absent across two frames must not shift any row below it — a pixel/cell-level
    regression test on frame layout, not a visual read).
  - Cards use collapsed borders exactly as §4.1 specifies: one outer `Rounded` border, each
    card right of the first draws `Borders::LEFT` only; `Padding::horizontal(1)` everywhere,
    Reader adds vertical 1; focus shown by border/title color only, never weight; a closed
    (fully-enclosed) border is always transient/overlay — asserted as a lint-style test over
    every `Block` construction site in `view.rs`.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (the six-row vertical stack at
  every height tier, header content and its exact narrowing drop order, banner reserved-row
  no-shift property, collapsed-border rule enforced across every card, heartbeat still
  excluded from `inflight`)

## 121. Lens strip / breadcrumb row
- [ ] status
- **depends-on:** 113, 119
- **parallel-safe:** no
- **acceptance:**
  - Row 2 renders the lens strip (§5.2) in Mail collections: every lens tab shows its
    auto-assigned jump chord (`'a`, `'u`, …), its honest count per task 113's state
    machine, and the right-side 4–6 key crib generated from the live keymap (not
    hand-written — drift is the same failing-test pattern §8.6 establishes for the keybar).
  - In non-mail collections and in zoomed Reader, the same row renders the breadcrumb
    instead (§5.2, §3.2): `work ▸ INBOX ▸ ⧉ subject ▸ message 2/4` — the live navigation
    stack, always visible, never requiring a keypress to reveal (law 5, no invisible
    state).
  - `<`/`>` cycle lenses, `''` flips to last (task 113's one-slot history), `'?` opens the
    full switcher via the finder `/` scope (wiring the finder call is task 142; this task's
    acceptance is satisfied if `'?` opens *a* finder scope stub that 142 completes).
  - At <25 rows this row is dropped per §4.3's height-tier table and folds into the List
    title instead (verified against `layout_mode`'s output from task 107, not a second
    height check invented here).
- **verify:** `cargo nextest run -p rmail-cli tui::view` (lens tabs render chord+count for
  every lens state from task 113, breadcrumb renders in non-mail/zoomed-reader contexts,
  `<`/`>`/`''` all move the active lens correctly, row drops below 25 and folds per
  `layout_mode`)

## 122. Sidebar card
- [ ] status
- **depends-on:** 107, 111, 119
- **parallel-safe:** no
- **acceptance:**
  - Renders the full §5.3 content top-to-bottom as one scrollable list with a cursor that
    skips section headers: ACCOUNTS (active marker `▎`, accent dot, `~unread` from the
    ledger), MAILBOXES (tree with `▸`/`▾` folds via `za`), QUEUES (Outbox/Follow-ups/
    Waiting-on/Drafts/Notifications, each rendering its jump chord and badge — `2·1✗` /
    `5·2!` overdue-marker format matches §5.3 exactly), VIEWS (saved searches + smart
    folders), TAGS (top-N by count with `Tag.color` dots), and the 14-day volume sparkline
    as a decorative footer.
  - `f` filters the tree in place (reusing task 110's filter engine over sidebar rows, not
    a second parser); Enter opens per row-kind (folder→collection, queue→its collection,
    view→runs as lens, tag→pivot) through task 119's registry — no sidebar-specific
    dispatch table duplicating it.
  - Per-account braille spinner while that account syncs; `!` glyph in `err` tone on a
    failed account — sourced from the existing per-account sync status the daemon already
    reports (task 92's heartbeat plumbing), not a new poll.
  - Folder unread counts follow the ledger's honesty rules from task 111 exactly (`~N` /
    `•`) — a test specifically asserts the sidebar never prints a bare integer for a folder
    unread count, closing the exact gap §5.3 calls out as a documented daemon limitation.
  - Renders at `Length(22)` per §4.2's M/L/XL breakpoints and as a left drawer at narrower
    ones (task 109's mechanism, not a second drawer implementation).
- **verify:** `cargo nextest run -p rmail-cli tui::view` (every §5.3 section renders with
  correct chord/badge format, `za` folds a section, `f` narrows via the shared filter
  engine, Enter dispatch for each row-kind through the collection registry, ledger honesty
  on folder counts, width at each breakpoint via `layout_mode`)

## 123. List card — title line, virtualized table, row anatomy
- [ ] status
- **depends-on:** 107, 119, 117, 118
- **parallel-safe:** no
- **acceptance:**
  - Title line renders exactly §5.4's k9s-style state string: collection name, active
    filter chip, active sort indicator, shown/total with `(partial)` when filtering a
    partially-loaded folder, live-stream marker (`⠿ live`).
  - Content is a virtualized `Table` building only the visible slice (`offset..offset+
    height` from `TableState`) — a test with a 100k-row synthetic collection asserts the
    per-frame build touches O(visible rows), not O(total), rows.
  - Row anatomy matches §6.1/§6.2 exactly: the mark-gutter + 4-glyph cluster (via task
    118's `Icons`) at fixed cell positions, and the **exact** column budget for every
    breakpoint tier in §6.2's table (≥96 XL cozy 2-line, 72–95 L compact, 56–71 M, 40–55 at
    the L-frame floor, <40 forced 2-line) — each tier's column widths literally sum to ≤
    inner width, asserted per tier, not just "doesn't panic".
  - Truncation is `unicode-width`-measured with `…`, never byte-sliced (a test with wide
    CJK/emoji subjects proves this); From/Subject end-truncate, addresses/message-ids
    middle-elide; tag chips are whole-or-`+N`, never mid-truncated.
  - Dates render per §6.3's exact tiering (`<24h`→`14:02`, `<7d`→`Tue 14:02`, same-year→
    `Aug 12`, older→`2024-08`) in `fg_muted`/`fg` (unread); scheduled/outbox rows show
    relative future (`in 2h`) in `scheduled` amber.
  - `zd` cycles compact/cozy/relaxed density, default compact (cozy at XL per §6.2),
    persisted per collection-kind via task 114's prefs store.
  - Scroll: 3-row scrolloff; `C-d`/`C-u` half-page with one-row overlap (reuses the exact
    `PAGE_OVERLAP`/paging behavior task 106 already proved, extended to the new card
    geometry, not reimplemented); nearing the loaded tail requests the next page via the
    existing `x-rmail-next-page-token` header and renders one ghost row
    `⠿ loading 500 more…`.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (title-line state string in every
  combination of filter/sort/partial/live, virtualized build cost on a 100k-row fixture,
  every §6.2 breakpoint's column-sum ≤ width, unicode-width truncation on wide-glyph
  fixtures, date-tier boundaries including the year rollover, density cycle persistence,
  scrolloff and page-overlap, tail-page ghost row)

## 124. List card — search-hit rows and time-bucket headers
- [ ] status
- **depends-on:** 123
- **parallel-safe:** no
- **acceptance:**
  - Search-hit rows add the §6.4 score meter (3-cell `▮▮▮`, one cell per `sources[]`
    agreement — lexical/dense/entity) replacing the chip zone, plus highlighted matched
    spans from `SearchHit.snippet.highlights` byte ranges rendered in `match_hl` bg+bold.
  - `2 similar` near-duplicate-collapse chip and `⧉N` server-thread-collapse chip both
    render per §6.4/§6.5, sourced from the fields the wire types already carry (no new RPC
    field needed — verified against the existing `search.proto` message before writing
    this task).
  - Time-bucket section headers (`TODAY`/`YESTERDAY`/`THIS WEEK`/month/year) render on
    date-sorted lists at ≥30 terminal rows per §6.5 — as non-addressable rows the cursor
    skips entirely (`j`/`k`/`gg`/`G` never land the cursor on one; a test walks the full
    list with `j` and asserts the cursor visits only real rows) — `fg_faint`, `─` fill; off
    below 30 rows and off below the 30-row height tier per §4.3.
  - Folder collections' `⧉N` thread chip counts only *loaded* rows sharing a `thread_id`
    (§6.5's "the daemon lists messages; client re-threading over partial pages would lie")
    — a test with a thread whose members span two pages asserts the chip undercounts
    honestly rather than guessing the true total.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (score-meter cell count matches
  `sources[]` length, highlight spans render at the correct byte-to-cell offsets under
  unicode content, time buckets skip-only-cursor behavior, bucket visibility gated on both
  row-count and height-tier, `⧉N` undercounts honestly across a split-page thread fixture)

## 125. List card — selection, cursor, marks
- [ ] status
- **depends-on:** 123, 110
- **parallel-safe:** no
- **acceptance:**
  - Cursor row renders full-row `bg_selection` + `▌` cap when its card is focused,
    `bg_select_blur` with no cap when not — never `Modifier::REVERSED` anywhere in this
    path (a source-scan test over the row renderer, matching the existing v1 convention).
  - `x` toggles mark (`✓` in the gutter, own cell, never colliding with a glyph — proven by
    a test asserting the mark cell and glyph cells never share a column index); `v` enters
    visual range (`▪` on every covered row as motions extend it); any verb applied in
    visual mode acts on the whole range and exits visual mode as one action, not one action
    per row followed by a manual exit. `X` clears all marks.
  - **Marks survive filtering and scrolling** (§6.6) — a mark set before a filter is
    applied (task 110/141) is still set after the filter hides its row, and the status zone
    (task 129) reads `3 marked (1 hidden)`; a bulk verb over marks that include hidden rows
    prompts exactly once: `includes 1 filtered-out message — proceed? y/n`, never silently
    acting on fewer than the user marked or silently including hidden ones without asking.
  - Unread rows render bold; read rows in muted lenses (e.g. News) render `fg_muted` — the
    lens-driven muting is a property of the active lens (task 113), not a per-row flag
    invented here.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (cursor styling focused vs. blurred
  with no `REVERSED` usage, mark/glyph cell disjointness, visual-range extend-then-apply-
  then-exit as one unit, marks-survive-filter with the exact hidden-count message and the
  one-time confirm prompt, muted-lens read-row styling)

## 126. Reader card — master-detail link, header block, AI capsule
- [ ] status
- **depends-on:** 107, 119, 117
- **parallel-safe:** no
- **acceptance:**
  - List cursor movement re-renders the Reader via a debounced (40 ms) generation-stamped
    `Mail.Get`, **cancel-on-scroll** — a test that moves the cursor 20 times within 40 ms
    asserts exactly one `Mail.Get` reaches the executor (not 20, not 0), reusing the
    existing stream-generation/supersession discipline rather than a bespoke debounce.
  - Cached headers render instantly on cursor move; the body region shows a 3-line shimmer
    skeleton until `Get` lands — never a blank frame (law 8).
  - Header block renders exactly §7.2's six weeded headers + relationship context (threads
    count, reply latency from the cached contact insight, absent silently when uncached —
    never a placeholder claiming data that isn't there); `i` toggles all raw headers inline,
    scrollable.
  - Injection-flagged messages insert the full-width banner directly under headers per
    §7.2's exact copy and trigger condition (`ScanInjection.actions_withheld`) — AI actions
    genuinely withheld in this state (not just visually flagged — a test asserts the
    suggested-reply/tag-accept affordances are actually disabled, not merely bannered over).
  - AI capsule (§7.3) reads the `<5 ms` `GetSummary` cache only — a test asserts opening
    the Reader on a message with no cached summary issues **zero** `AnalyzeMessage` calls
    (never a model call by itself); renders `⠋ pending`/`✗ failed — ! retries`/`✦ analyze
    (!) — $` per state; all model text `«»`-quoted in `ai` tint (law 9); `(local)` chip
    when the local model produced it.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (debounce-and-cancel-on-scroll
  coalesces to one `Get`, skeleton-then-swap with no blank frame, six-header block content
  and relationship-hint absence-when-uncached, injection banner condition and actual
  affordance-disabling, AI capsule zero-model-call-on-cache-miss and all three state
  renderings)

## 127. Reader card — body rendering rules
- [ ] status
- **depends-on:** 126, 116
- **parallel-safe:** no
- **acceptance:**
  - Measure clamp exactly per §7.1: `inner_width≥80` → `measure=min(inner_width−8,100)`
    centered; `<80` → `measure=inner_width−2`, no centering. Applies identically to message
    bodies, Digest, Ask answers, and the Manual (a shared function, not four copies) —
    §1.2's mandated grafted fix, now a literal never-exceeds-100 / never-below-72-when-
    affordable test rather than the "wraps at 132" defect §1.3 lists.
  - Pre-wrapped via `textwrap` into cached `Vec<Line>` through task 116's wrap cache keyed
    `(message_id, width, fold_state)` — not a fresh wrap per frame.
  - Quotes: leading `>`-runs become a `▎` gutter, depth-colored `quote1..4` cycling; text
    `fg_muted`; blocks >4 lines fold to `▸ 12 quoted lines — za`, expansion remembered per
    message; `zq` folds/unfolds all quotes at once. Signatures (RFC 3676 `-- ` + heuristic)
    render `fg_faint`, folded to one rule by default, `zs` toggles; legal-footer heuristic
    likewise. Attribution lines render `fg_faint` (structure, not content).
  - HTML mail prefers a non-trivial `text/plain` part; else daemon-extracted text + `html ·
    H opens browser` chip — **no inline HTML engine**, confirmed by there being no new HTML
    rendering dependency added anywhere in this task's diff (a `Cargo.lock` diff check, not
    just a promise).
  - Links: inline `[n]` markers at their spans from `ExtractLinks`; bottom LINKS strip
    ordered by value score with kind chips and `⚠ spoofed-host` on `deceptive`; `gl` hint
    mode echoes the URL in the status zone **before** opening (a test asserts the status
    zone is written before any browser-open `Cmd` is issued, not after — this is the
    phishing-defense property, and ordering is what makes it real); a `deceptive` link
    additionally requires one extra `y`. `y`+number copies via OSC 52 + arboard with
    `copied ✓` confirm. OSC 8 hyperlinks emitted additionally as progressive enhancement.
  - Attachments strip: one row per attachment; `a` opens the attachment-browser overlay
    with the arbitration-table verbs (Enter open, `s` save streamed with jobs-feed
    progress, `v` view image via `ratatui-image` in its dedicated overlay only — never
    in-flow, `t`/`i` extract, `?` ask-$) — Report overlays for results carry per-field
    provenance (`parsed` vs `«model»`).
  - Entities underlined in `entity` color at their spans, Enter-pivotable. Notes block
    (`¶ NOTES`, markdown, newest-first, 6-line collapse, `Space n n`/`Space n e`, live
    `WatchNotes` refresh). Thread strip: collapsed one-line prior messages above the body,
    Enter expands in place, `[`/`]` walk the thread.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (measure clamp shared across all
  four consumers with boundary widths 79/80/172/173, wrap-cache reuse across re-renders at
  the same key, quote-depth cycling and fold/unfold, signature fold default + `zs`, no new
  HTML-rendering dependency, link-hint status-before-open ordering and deceptive-link extra
  confirm, attachment-browser verb table, notes live-refresh, thread-strip expand/walk)

## 128. Rail card — tabbed context
- [ ] status
- **depends-on:** 126
- **parallel-safe:** no
- **acceptance:**
  - Tabbed `[✦ AI] Thread Ents Contact Why Ask` per §5.6; always about the cursored row's
    message, sharing the exact same 40 ms debounce/cancel-on-scroll discipline as the
    Reader (task 126) — not a second, slightly different debounce implementation.
  - `[`/`]` cycle tabs when rail is focused; direct jumps `ge`/`gc`/`w`/`A`/`ga` land on the
    named tab from anywhere (through task 119's goto-chord registry, not a rail-local
    dispatch).
  - Thread tab: `GetThread` timeline (who/when/first-line), participant affinity, `«thread
    summary»` when cached, waiting-on verdict; `j/k` select, Enter jumps. Ents tab:
    message entities, Enter = `SearchEntities` pivot into the List (reusing task 119's
    collection registry to load the pivot result, not a bespoke navigation). Contact tab:
    `GetContactInsight(metrics_only)` card, Enter → full contact page (built in task 162;
    this task's Enter may target a registry stub it completes). Why tab: rank explanation
    for search hits (full implementation in task 140's search work; this task wires the tab
    and its empty/non-search state honestly — "no ranked row" rather than a blank pane).
    Ask tab: the RAG pane skeleton (full implementation task 171).
  - `\` toggles the rail; at <L breakpoint it opens as a drawer via task 109's mechanism.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (tab cycling and direct-jump chords,
  shared debounce/cancel proof against the Reader's, Thread/Ents tab content and pivot
  navigation, Contact tab's cached-metrics card, Why/Ask tabs' honest empty states, `\`/
  drawer behavior)

## 129. Status bar v2
- [ ] status
- **depends-on:** 107, 112
- **parallel-safe:** no
- **acceptance:**
  - Fixed zones left→right exactly per §5.7: `MODE` (derived, unchanged mechanism from
    task 92) · scope (account/collection + filter + sort echo) · marks (with hidden
    accounting from task 125) · **message zone** (the only flexing zone; errors land here,
    stick until a keypress, and are *also* appended to the notification feed — a test
    asserts both effects happen from one error, not either/or) · **undo-send chip**
    (never evictable — task 146 supplies its content; this task guarantees the zone itself
    can never be pushed off by anything else narrowing) · inflight `⧗N` · daemon glyph
    cluster (mirrors the header when the header is folded, sourced from the same state, not
    a second poll) · pending chord/count.
  - Narrowing drop order matches §4.3's status-bar row exactly: daemon glyph cluster →
    marks → scope; the message zone keeps its documented `MIN_MESSAGE` floor; the undo-send
    chip is **never** dropped at any width (a test sweeps every width down to the minimum
    supported terminal and asserts the chip is still present when armed).
  - List-title filter-chip drop order (§4.3's third drop-order table) is also implemented
    here since it's the same "nothing silently vanishes" mechanism: filter is the last
    thing dropped from the title, and if it must go, the status scope zone shows `</f>` as
    a fixed 3-cell reminder that a filter is still active.
- **verify:** `cargo nextest run -p rmail-cli tui::status` (zone order and content, message
  zone dual-effect on error, undo-chip un-droppable sweep, daemon-glyph header/status
  mirror consistency, `</f>` reminder when the filter chip itself is dropped)

## 130. Keybar v2
- [ ] status
- **depends-on:** 107
- **parallel-safe:** no
- **acceptance:**
  - Renders the 8 highest-value keys for the focused card/collection, re-derived on every
    focus change, generated from the live `Keymap` + verb registry — the existing drift-
    test pattern (§8.6 point 1) extended to be collection-aware (a folder's 8 differ from
    the outbox's 8, both derived, neither hand-written per-collection).
  - Menus/pickers additionally display each row's direct key inline (the lazygit rule,
    §5.8) — a test over every existing overlay's row-render path asserts a bound key is
    shown when one exists.
  - Row dropped entirely below the 25-row height tier per `layout_mode` (task 107), same
    mechanism as the lens strip (task 121), not a second height check.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (8-key selection differs correctly
  across at least three collection kinds and is regenerated on focus change, drift test
  against the live keymap, direct-key-inline on overlay rows, height-tier drop)

## 131. Toast float, notification feed, jobs feed
- [ ] status
- **depends-on:** 112
- **parallel-safe:** no
- **acceptance:**
  - Toasts float bottom-right over the card area (`Clear` + 1-line block), taking **no**
    layout row (§4.1/§12.4) — a test asserts adding/removing a toast never changes any
    other widget's `Rect`. One visible + `+N` badge, queue of 5; Undo > priority > newest;
    a live Undo is never evicted; TTL driven by countdown `Cmd`s, never a free-running tick
    (reuses task 92's existing "no tick without a live spinner/countdown" discipline).
  - Notification feed (`gn`, reached through task 119's registry as a collection): durable,
    resumable `StreamAlerts since_id` merged with local error/toast history; tier-colored
    rows with `«reason»`; Enter opens the message; `w` explains via `ScoreMessage`
    (threshold, suppression, would-notify verdict) — every toast that was ever shown is
    also findable here (a test creates a toast, lets it expire, and finds it in the feed).
  - Jobs feed (`gj`, same registry mechanism): background operations (exports, attachment
    saves, reindex drains, bulk actions) each with a `LineGauge`, cancel key, outcome row.
    Missing done-sentinels (`ExportDone`, `IndexProgress.done`) are reported as **cut
    short**, never as success — a test that terminates a job stream without its
    done-sentinel asserts the row reads "cut short", not a green checkmark.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (toast float takes no layout row,
  eviction priority order, TTL via countdown only, notification feed resumability and
  toast↔feed correspondence, jobs feed done-sentinel honesty on a truncated stream)

## 132. Modal engine v2 — card-focus semantics
- [ ] status
- **depends-on:** 107, 109
- **parallel-safe:** no
- **acceptance:**
  - Resolves §1.3's headline defect table for this design: `h`/`l`/`Tab` are **always**
    card focus (never a panel-specific meaning), and digits are **always** counts (never a
    sidebar-focus shortcut) — both proven as the single dispatch rule in `on_key`, not a
    per-mode special case that could regress later.
  - `h` from the Reader card moves focus to the List (§1.3: "Reader is the promoted third
    card, so `h` from it lands on the list — same muscle motion, one rule"); pop remains
    exclusively `Esc`/`q` (task 115's ladder) — a test specifically re-creates the exact
    v1-Cockpit-draft defect (`h`=focus-left in list but pop in Reader) and asserts it cannot
    recur: `h` in the Reader never pops.
  - `Enter` promotes focus rightward along master→detail (§3.2): list row → Reader focused
    with full message rendered; outbox row → entry detail; citation → cited message
    (pushes the breadcrumb). One function implements "promote," parameterized by the
    focused collection's detail renderer (task 119's trait), not one `Enter` handler per
    collection.
  - `C-w h/j/k/l` gives explicit directional focus as an alternative to `h`/`l`/`Tab`,
    resolving to the same focus-change code path (proven by a test that both produce
    identical `Model` deltas for the same starting state).
  - Decides and implements the zoom/focus interaction rule task 109 left narrow: task 109's
    `toggle_sidebar`/`toggle_rail` clear `Model::zoom` whenever their focus-summon branch
    moves focus onto a card the current layout would otherwise hide, specifically so the
    summoned card is never left invisible behind an unrelated zoom (`layout_mode`'s zoom
    branch answers only `ctx.zoom`, never `ctx.focus`). This task is what gives `h`/`l`/`Tab`
    a *general* way to move focus onto a hidden card, so the same question recurs for every
    one of them, not just the two `\`/`C-b` already answer. Resolve it as one rule, not
    per-key: moving focus onto a card the layout would otherwise hide always clears zoom;
    moving focus between two cards that are already both visible in the current layout does
    not. §4.5's "survives focus changes and resizes" governs the second case only — a bullet
    and a test say so explicitly, so this is decided once rather than re-derived per key.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (h/l/Tab focus-only semantics
  under every card and zoom state, digits-are-always-counts even with a card named by a
  digit-adjacent chord, the h-in-Reader regression test, Enter's promote-rightward for at
  least three different collection kinds via the trait, `C-w` directional parity)

## 133. NORMAL keymap — List card core verbs
- [ ] status
- **depends-on:** 119, 132
- **parallel-safe:** no
- **acceptance:**
  - Every core verb in §8.2's List-card table is bound and dispatches through `run_verb`
    (the existing single dispatch path — no key does anything a typed `:` line could not):
    `j/k`, `gg/G`, `C-d/C-u`, `Enter`, `e`, `d`, `D` (confirm), `m`/`M`, `U`, `s`, `r/R/F`,
    `c`, `t/T`, `u` (task 112's stack), `b`, `f` (task 141), `/` (task 140), `C-p`, `:`,
    `.`, `\`, `C-b`, `A`, `w`.
  - Optimistic rendering for flag/tag/move/archive/delete: the row updates (slides out
    for archive/trash/delete) on the **same frame** as the keypress, before any RPC
    response — a test drives the key, inspects the very next rendered frame, and asserts
    the row is already gone/changed, with reconciliation via `WatchEvents` proven
    separately as a no-op when the optimistic guess matches the eventual truth.
  - Refusal path: an RPC error rolls the optimistic change back **visibly** and posts
    `err` toast naming the RPC and `Status` code, with `r retry` using the same idempotency
    key (not a fresh mutation) — a test forces a `PERMISSION_DENIED` and asserts the row
    is back exactly where it was, not merely "not gone".
  - `Reader`/`Visual` inherit this table through `Normal` → `Global` chaining (the existing
    layer mechanism), proven by a test that every List-card verb also resolves correctly
    when the Reader card is focused and the action is meaningful there.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (every listed verb dispatches
  through `run_verb`, same-frame optimistic render for each mutating verb, visible
  rollback + named-RPC toast + idempotent retry on refusal, layer inheritance into
  Reader/Visual)

## 134. Chord families — `g` `p` `o` `y` `z` `'` and leader `Space`
- [ ] status
- **depends-on:** 133
- **parallel-safe:** no
- **acceptance:**
  - `g` goto chord: every target in §8.2's list (`gm gu go gf gw gn gj gd gv gi gr gs gh ge
    gc ga gt gl g/ g1..g9 gx`) resolves through task 119's collection registry where
    applicable, or a named direct action otherwise (`gl` link hints, `g/` manual grep,
    `g1..g9` account switch, `gx` index status) — no goto target is a dead key.
  - `p` pivot chord: `pt ps pd pr pc pg pe` each open a pre-filled search collection with
    the pivot recorded as provenance (title reads `pivot ▸ <query>`; Esc pops back;
    chained pivots show the full chain in the breadcrumb) — §8.2's exact query construction
    per pivot kind (`pd`→`from:@domain`, `pc`→`from:X OR to:X`, etc.) is tested literally
    against the compiled query string, not just "a search happened".
  - `o` sort chord: wired in this task to dispatch (full sort semantics land in task 143;
    here the chord resolves to the right sort-target action for `od of os oz op ou or oo
    ot`, proven reachable, with `143` owning correctness of the resulting order).
  - `y` yank chord (`ya ys ym yl yq yp`): OSC 52 + arboard, `copied ✓` in the status zone —
    every sub-key copies the exact field §8.2 names, tested against fixture data with
    punctuation/unicode in the copied field to catch a naive `format!` truncation bug.
  - `z` view/fold chord (`za zq zs zd`): dispatches to the exact fold/density behaviors
    already specified in tasks 122 (sidebar `za`), 127 (`za`/`zq`/`zs` on the Reader), 123
    (`zd` density) — this task's job is proving the chord resolves to the *same* handlers,
    not reimplementing folding a second time.
  - `'` lens-mark chord and `''` flip dispatch into task 113's lens engine exactly.
  - `Space` leader mirrors the full command tree per §8.2's group list — every leaf is a
    real verb reachable by `:` too (the existing `every_tui_action_is_a_capability_or_
    declared_local` drift check, exercised against every new leaf this task adds).
- **verify:** `cargo nextest run -p rmail-cli tui::model` · `cargo nextest run -p rmail-core
  keymap::` (every `g`/`p`/`y` sub-chord's exact effect including compiled pivot queries
  and copied field content, `z`/`'` chords delegate to the already-tested handlers without
  duplicating logic, leader tree fully reachable and every leaf capability-backed)

## 135. The arbitration table (§8.3)
- [ ] status
- **depends-on:** 134
- **parallel-safe:** no
- **acceptance:**
  - Every row of §8.3's table is implemented exactly as specified, and — this is the
    task's real point — **nothing not listed in that table deviates from law 2** (one
    meaning per key): a test enumerates every key this build binds to more than one
    `Action` depending on context and asserts the enumeration is a subset of §8.3's table,
    by key. A new context-dependent binding added later without a matching table row and a
    matching CLAUDE.md-style justification fails this test by construction, not by review
    diligence.
  - The specific resolved conflicts are each individually tested: `o` sort vs. rail-✦AI
    cycle-model vs. visual-mode swap-ends; `s` star vs. attachment-save vs. outbox
    send-now; `e` archive vs. outbox-edit-body vs. invoice-CSV-export; `t` tag-palette vs.
    outbox-reschedule; `R` reply-all vs. outbox-retry; `u` undo vs. outbox-cancel-scheduled
    (proven to be the *same* underlying undo-stack operation, per §8.3's own rationale, not
    two different code paths that happen to look similar); `a`/`x` attachment-browser vs.
    accept/reject-suggestion; `!` force/re-analyze; `w` why-ranked vs. inert-on-waiting-on
    rows (proven inert, not silently doing something else).
  - The §8.3 footnote resolutions are each a test: archive=`e` not `a`; mark=`x` so
    why-ranked=`w`; `/`=search with finder on `C-p`; `u`=universal undo; suggested-reply
    opens via Enter-on-its-row rather than a global key; `zt`≡`]r`.
- **verify:** `cargo nextest run -p rmail-core keymap::` · `cargo nextest run -p rmail-cli
  tui::model` (the context-dependent-key enumeration ⊆ §8.3 table test, each named
  conflict's three-way resolution individually, all six footnote resolutions)

## 136. Which-key v2
- [ ] status
- **depends-on:** 134
- **parallel-safe:** no
- **acceptance:**
  - Instant-on-pending-chord band, grouped by chord family, matches the existing v1
    mechanism's proof obligations (task 91: a pure render-time function of existing state,
    no new timer) extended to every new chord family task 134 adds.
  - Shadowed entries (a binding overridden by a user's `keys.toml`) render struck-through
    rather than being omitted — a user can see what they gave up, not just what they kept.
  - Overflow renders `+N more (?)` when a group has more members than the band's row
    budget allows, and `?` from that state opens the full help overlay pre-filtered to the
    pending prefix (not a generic help open that loses context).
- **verify:** `cargo nextest run -p rmail-cli tui::whichkey` (grouping by the new chord
  families, struck-through shadowed entries, overflow threshold and `+N more (?)`
  behavior, `?`-from-overflow context preservation)

## 137. Teaching hints
- [ ] status
- **depends-on:** 133
- **parallel-safe:** no
- **acceptance:**
  - After three consecutive uses of a slow path for the same action within a session (the
    canonical example: typing `:archive` three times), a one-line status hint names the
    direct key (`tip: e archives — :set hints off to silence`) per §8.6.
  - Rate-limited to one hint per action per session — a fourth, fifth, sixth slow-path use
    of the same action produces no further hints, tested explicitly (not just "eventually
    stops").
  - `:set hints off` silences all teaching hints for the session, persisted via task 114's
    prefs store as the `hints` field.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (three-strikes trigger with the
  exact message format, one-hint-per-action-per-session cap proven past the fourth use,
  `hints off` persistence and effect)

## 138. Autocomplete popup engine (unified)
- [ ] status
- **depends-on:** 132
- **parallel-safe:** no
- **acceptance:**
  - New shared popup anatomy (max 8 rows, opens adjacent to the input, `Tab` accepts,
    `↑↓` move, typing filters, matched characters highlighted from positions, dim
    right-aligned annotation for kind/description/resolved-value) implemented **once**
    and reused by every surface in §8.7's table, not duplicated per surface — a test
    instantiates the popup against fixture data from at least four of the ten listed
    surfaces (search operators, `:` command line, compose To/Cc, tag palette) and asserts
    identical rendering/interaction behavior modulo the source list.
  - Each surface's ranking rule from §8.7's table is implemented as a pluggable ranker
    (prefix / frecency / count-desc / tree-order / 5-tier / fuzzy / dir-first), not a
    single hardcoded order — tested per surface against its documented ranking.
  - Time-input surfaces (`C-l`, `b`, reschedule) additionally echo the **daemon-resolved
    absolute time live** under the input from a dry-resolve call — a test asserts the echo
    updates on each keystroke debounce, not only on submit, and that it reflects the
    server's resolution rather than a client-side `chrono` guess when both are available.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (shared popup behavior identical
  across ≥4 surfaces, per-surface ranking correctness for all seven ranking kinds, live
  time-echo updates per keystroke sourced from the dry-resolve RPC)

## 139. Rebinding v2 — keys.toml shadow lint + `c` rebind flow
- [ ] status
- **depends-on:** 135
- **parallel-safe:** no
- **acceptance:**
  - `keys.toml` hot-reload (1 s poll, unchanged cadence) and shadow lint both extend
    cleanly over every new Action this part adds — the existing `:keys check` surface and
    status-line warning need no new mechanism, only a bigger `Action` enum to validate
    against (proven by re-running task 105's existing shadow-lint tests unmodified against
    the v2 keymap and asserting they still pass).
  - The unbindable set from task 115 (`Esc`, `Ctrl-C`, `:`, and the double-`Ctrl-C` quit)
    is refused by `SetBinding`/file-parse with a **named reason**, not a generic parse
    error — a user attempting to rebind `Ctrl-C Ctrl-C` sees why, not just that it failed.
  - `c` in the help overlay opens the rebind flow, which calls
    `ConfigService.GetKeymap`/`SetBinding` (already-existing RPCs, verified present in
    `config.proto`) — the flow round-trips a rebind to the file and confirms the new
    binding is live on the next `keys.toml` poll without a restart.
  - Kitty keyboard protocol enhanced chords (`S-Enter`, `C-S-x`) are bonuses with legacy
    equivalents; the protocol flags are pushed at startup and popped on **both** clean exit
    and panic (a test installs a panic hook and asserts the pop runs — this is the one
    terminal-state correctness property that, if missed, corrupts the user's shell after a
    crash).
- **verify:** `cargo nextest run -p rmail-core keymap::` (unbindable-set named refusals,
  existing shadow-lint suite green against the expanded `Action` enum) · `cargo nextest
  run -p rmail-cli tui::` (rebind-flow round-trip through `GetKeymap`/`SetBinding` against
  the in-process daemon harness, kitty-protocol push/pop-on-panic)

## 140. Search v2
- [ ] status
- **depends-on:** 119, 123, 128
- **parallel-safe:** no
- **acceptance:**
  - `/` transforms the List card in place (no modal takeover) per §9's exact frame: prompt
    row under the list title, hits streaming best-first below, Reader following the top hit
    until the user moves, `w` flips rail to Why.
  - Incremental: 25 ms debounce, cancel-prior-stream via generation + daemon single-query
    slot, **old hits stay visible dimmed until the first new batch** (no strobe) — a test
    types three fast keystrokes and asserts only the final query's stream survives and the
    displayed rows never flash empty in between.
  - Full operator/sigil grammar (§9.2) parses; `Tab` completes operators then values
    through task 138's popup wired to the right source per operator (contacts/tags/
    folders); unknown `key:value` degrades to free text, **never errors**.
  - `C-n` compiles NL via `CompileQuery`; renders the confirm strip (raw → compiled DSL,
    per-operator lines, `«model note»`, `cached` badge); `Enter` runs it, `e` edits the DSL
    — never silently guessed (a test asserts an NL query with no `Enter` never issues the
    underlying `Search` call).
  - Why-ranked (`w`) rail content: feature-contribution block meters that **sum exactly to
    the score** (a literal arithmetic assertion, not "looks close"), retriever sources,
    matched span, `«claude_reason»` when L2-reranked; identical content to CLI `--explain`
    (the three-parity rule — a shared test fixture run through both surfaces and diffed).
    Explain failures latch visibly (`w!`), never silently.
  - `Enter` pins the result set as the collection (breadcrumb `search ▸ "query"`) so `J/K`
    walk hits and every verb works, through task 119's collection trait — search becomes
    a first-class `Collection`, not a special-cased mode.
  - Degradation badges render exactly per §9.7 (`semantic off`, `indexing… lexical
    fallback`, `try ~semantic?`); `LogFeedback` fires when `query_id≠0` with the footer's
    one-time `feedback logged` notice; Esc aborts the stream (ladder step 3, reusing task
    115) and restores the previous collection from kept rows, not a fresh reload.
  - `ot` toggles server-side `thread_collapse`; collapsed rows show `⧉N`, `za` expands
    inline members from `thread_collapsed[]` — reusing the same `za` fold mechanism task
    134 already wired, not a second fold key.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (debounce/cancel/no-strobe,
  operator grammar parse table, NL-compile confirm-before-run, why-ranked score-sums-
  exactly, three-parity fixture diff against CLI `--explain`, Enter-pins-as-collection,
  every degradation badge condition, Esc-restores-from-kept-rows, `ot`/`za` thread-collapse)

## 141. Filter `f` wiring
- [ ] status
- **depends-on:** 110, 140
- **parallel-safe:** no
- **acceptance:**
  - `f` is card-scoped exactly per §10: List card narrows rows (task 110's engine), Reader
    card finds-in-message (distinct from `/`, per §7.1 — a test asserts `f` inside the
    Reader never issues a `Search` RPC), Sidebar filters the tree (task 122's `f`, proven
    to be the *same* call as this task's, not a coincidence of naming).
  - An unsupported operator (task 110's `Unsupported` classification) renders red inline
    with `use / for that`, exactly the copy §10 specifies — and the overlay/prompt stays
    open rather than closing on the rejected keystroke (a test types an unsupported
    operator and asserts the prompt is still focused and editable afterward).
  - `C-Enter` escalates the current filter text into a real `/` search verbatim — the
    exact string, not a re-derived approximation (a test with a filter containing a
    negation and a quoted phrase asserts the escalated search query is byte-identical to
    the filter's text).
  - The `(partial)` chip + "`/` searches all" hint render together on a partially-loaded
    folder per §10's honesty row.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (card-scoped dispatch to three
  different behaviors, unsupported-operator red-inline-and-stays-open, `C-Enter` verbatim
  escalation, partial-folder honesty pairing)

## 142. Finder v2 reconciliation
- [ ] status
- **depends-on:** 132
- **parallel-safe:** no
- **acceptance:**
  - Existing finder (`C-p`, `FinderService.Find` streamed batches) keeps every proven v1
    property — sigils (`> # @ / :`), scope cycling (`C-p`/`M-p`), `Tab` multi-select +
    `C-a` select-all + `BatchAction`, kind glyphs, `indexing…`/superseded badges,
    empty-query recents ranking — reconciled against the new card/overlay-stack model
    (task 108) so it opens as a proper stack entry rather than the old single-slot overlay.
  - `'?` (task 121) and `Space`-leader finder entries open the finder pre-scoped to the
    right sigil rather than generic — a test drives `'?` and asserts the finder opens
    already scoped to lenses/collections, not requiring the user to type the sigil.
  - Snapshot-batch feel is preserved (batches replace, never strobe) — the exact property
    §10's table claims, re-verified against the stack-based overlay rendering path since
    that's what changed in this task, not the finder's own streaming logic.
- **verify:** `cargo nextest run -p rmail-cli tui::overlays` (every listed v1 property
  still holds against the overlay-stack integration, `'?` pre-scoping, batch-replace-no-
  strobe re-verified post-migration)

## 143. Sort chord + zoomed-table headers
- [ ] status
- **depends-on:** 119, 114
- **parallel-safe:** no
- **acceptance:**
  - `o` chord's sub-keys (`od of os oz op ou or oo ot`) each apply the documented sort;
    pressing the active mode's key again reverses — tested for every one of the nine,
    including that `or` (relevance) is only reachable/meaningful on search collections
    (inert with an honest status message elsewhere, not silently applying date-sort).
  - List title always carries the active indicator (`↓date`/`↑from`, §11).
  - **Zoomed List** (task 109's zoom mechanism) renders as the headed `Table` with real
    column headers (`glyphs · from · to · subject · category(ai) · size · date` + account
    chip in unified) and a sort arrow on the sorted column — the **only** header-click/
    header-arrow surface in the whole design, confirmed by a test that no other card ever
    renders a clickable/indicated column header.
  - Folder sorts operate on loaded rows only; title appends `(sorts 1,204 loaded — G loads
    more)` when the folder exceeds the loaded count; `o!` forces full pagination first with
    a progress toast, refused above 5k with a named reason (not a silent cap). Search
    results honor server-side sort where the plan allows (a test with a `QueryPlan`
    declaring server sort asserts the client does not re-sort on top of it).
  - Sort persists per collection-kind via task 114's prefs store, write-through.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (all nine sort keys plus
  reverse-on-repeat, relevance-sort collection-gating, zoomed-table headers+arrow as the
  sole such surface, loaded-vs-total honesty string and `o!`'s 5k refusal, server-sort
  deference, per-collection-kind persistence)

## 144. Stale-while-revalidate + skeletons
- [ ] status
- **depends-on:** 123
- **parallel-safe:** no
- **acceptance:**
  - Switching folders/collections keeps old rows visible, dimmed, with a `↻` title spinner
    until page 1 of the new collection lands, then swaps in place — a test asserts the
    frame immediately after a folder switch still renders the *previous* folder's rows
    (dimmed), never a blank list (law 8).
  - First-ever load (nothing was ever loaded for this collection) renders 8 shimmer
    skeleton rows (`░░░` in `fg_faint`) at plausible column widths for the current
    breakpoint — distinguished from the dimmed-old-rows case by a test that a collection
    with zero prior state never shows dimmed rows (there's nothing to dim).
- **verify:** `cargo nextest run -p rmail-cli tui::view` (dimmed-old-rows-until-swap on a
  collection switch with prior state, skeleton-rows-only on a never-loaded collection, no
  blank frame in either transition)

## 145. Ambient/detailed-tier Reports
- [ ] status
- **depends-on:** 120
- **parallel-safe:** no
- **acceptance:**
  - Each header gauge's expanding verb (task 120's `SYNC`/`IDX`/`AI`/`OUT`/`NET`) opens its
    detailed Report on Enter after focusing it via the finder `>` scope or the leader —
    reusing the existing `Report`/`ReportPane` engine, not a new detail-view mechanism.
  - Sync detail: per-folder table. Index detail: coverage `LineGauge` per kind from
    `IndexKindStatus.coverage`, with lag + quarantine counts. AI detail: queue/spend. Cache
    stats included where the underlying RPC already reports them — no new field invented
    on the client to fill a gap the daemon doesn't report (that's a §19 gap if it's
    missing, not something to fake here).
- **verify:** `cargo nextest run -p rmail-cli tui::report` (each of the five gauges opens
  the correct Report content via the finder `>`-scope path, index coverage gauge reads
  `IndexKindStatus.coverage` per kind including lag/quarantine)

## 146. Undo-send status chip
- [ ] status
- **depends-on:** 112, 129
- **parallel-safe:** no
- **acceptance:**
  - The status-bar chip (not a toast) renders `⏱ sending to Sara in Ns — u cancels`,
    counting down from `undo_deadline`; `u` calls `CancelScheduled` (absent id = most
    recent cancelable, reusing task 112's stack) and reopens the composer with draft +
    cursor position intact — a test asserts the reopened composer's cursor offset matches
    where it was when send was triggered, not reset to 0 or end.
  - The chip is driven by countdown `Cmd`s, never a free-running per-second tick unrelated
    to an armed window (task 92's existing discipline, applied here).
  - Confirmed never-evictable per task 129's acceptance — this task supplies the content,
    129 already proved the slot can't be pushed off.
- **verify:** `cargo nextest run -p rmail-cli tui::status` (countdown content and tick
  source, `u`-cancels-and-reopens-with-intact-cursor, absent-id-picks-most-recent
  semantics)

## 147. WatchEvents cursor-stability + pulse tint
- [ ] status
- **depends-on:** 123
- **parallel-safe:** no
- **acceptance:**
  - **The cursor never moves because of a network event** (§12.8, law-adjacent) — a test
    delivers a `WatchEvents` insert-above-cursor event and asserts the cursor's *logical
    row* (the message it was on) is unchanged, even though its *screen position* may shift
    if rows above it changed count.
  - Inserted rows land in place with a 2 s pulse tint, driven by a countdown `Cmd` (not a
    free tick) that clears itself — a test asserts the tint is present immediately after
    insert and absent after the countdown fires, with no lingering timer after that.
  - `WatchEvents` resumes from stored `since_seq`; `OUT_OF_RANGE` triggers resync from
    `resume_from` plus a toast `replayed N events` — a test simulates an `OUT_OF_RANGE`
    response and asserts both the resync call and the toast, not just one.
  - Events are a dirty flag → coalesced reload, never one reload per event (a burst of 50
    events in one tick coalesces to one reload, tested with a counting fake executor).
- **verify:** `cargo nextest run -p rmail-cli tui::model` (cursor-stability under an
  insert-above event, pulse-tint lifecycle exactly 2s via fake clock, `OUT_OF_RANGE`
  resync+toast pairing, event-burst coalescing count)

## 148. Frame discipline & perf instrumentation
- [ ] status
- **depends-on:** 120
- **parallel-safe:** yes
- **acceptance:**
  - Event-driven redraw only: a dirty-flag mechanism gates `terminal.draw`, and spinner/
    tick-driven redraws run **only** while something is actually animating (task 92's
    existing rule, audited across every new gauge/spinner this part adds — a source-scan
    test enumerating every `Cmd::Tick`-equivalent producer and asserting each is gated on a
    live condition).
  - Frame build+diff budget instrumented (debug-assert or a feature-gated timing harness)
    against the 4 ms typical / 16 ms worst budget from §18 — not enforced as a hard test
    failure (CI hardware varies) but logged via `tracing` when exceeded, so a regression is
    discoverable without being a flaky gate.
  - Synchronized-update (DEC 2026) wraps writes where the terminal advertises support,
    falling back cleanly where it doesn't (a test with a fake terminal capability set
    exercises both paths).
  - `Table`/`List` visible-slice building (task 123's virtualization) and pre-wrap caching
    (task 116) are both re-confirmed live on this pass — this task is the integration point
    that would catch either regressing silently as more cards/collections landed on top of
    them.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (every tick producer gated on a
  live-animation condition via source scan, synchronized-update capability branching, a
  smoke test confirming task 123/116's virtualization and caching still hold end-to-end)

## 149. Compose v2
- [ ] status
- **depends-on:** 107, 112
- **parallel-safe:** no
- **acceptance:**
  - Full-frame app per §14's exact layout: THIS DRAFT sidebar (reply target, revision
    cycle, autosave tick) persists through the compose session; rail becomes the Guardian.
    Entered via `c`, `r`/`R` (threading headers frozen at reply time by `CreateDraft`; a
    Reader visual selection quotes only the selection), `F`, Enter on a draft row, or Enter
    on a suggested-reply row (pre-streamed content already in the buffer on open).
  - Fields: `Tab`/`S-Tab` move; To/Cc/Bcc autocomplete is frecency-ranked via task 138's
    popup (fragment + initials matching, `jsm`→John Smith); `C-f` cycles From identities
    (signature + sent-folder re-derive on switch); `C-a` attach via filesystem completion
    (task 138's dir-first ranker).
  - Body: inline editor for short replies (multi-line, kill ring, bracketed paste = one
    undo unit); `C-e` suspends to `$EDITOR` with full repaint on return — a test drives
    suspend/resume against a fake `$EDITOR` and asserts the terminal is fully repainted,
    not left in a torn state. Autosave via `UpdateDraft` debounced 2 s; Esc = save + close,
    never a silent discard.
  - `C-g` AI menu streams generated text token-by-token into the **real editable buffer**
    (Esc mid-stream keeps text so far); renders `«ai-tinted»` until first hand-edit; `C-o`
    cycles revisions via `ListDraftRevisions` with hand-edits written back before
    switching; `(local)` chip on local-model output.
  - Guardian: `PreflightCheck` on field blur and always before send; BLOCK stops send
    (`:send!` bypasses explicitly, never implicitly); WARN requires one extra Enter; NOTICE
    lists informationally. Model findings never block (wire contract) and carry `«model»`.
  - Send plan exactly per §14: `C-s` = schedule-now + undo window (chip counts down, `u`
    cancels+reopens with draft+cursor intact — reusing task 146's exact mechanism, not a
    parallel one); `C-l` Send Later (preset chips + NL with the daemon-resolved absolute
    time echoing live via task 138's time-input mechanism); `C-t` Optimal
    (`SuggestSendTime` + shown `«rationale»`); `C-u` cycles the undo window, lengthen-only;
    `C-r` toggles `↩? remind in 3d if no reply` (`CreateFollowup`, cancel-on-reply).
  - Narrow terminals keep the single column; Guardian folds to a one-line strip
    (`GUARDIAN ⚠ 1 NOTICE — C-p details`) rather than disappearing.
- **verify:** `cargo nextest run -p rmail-cli tui::commands` (layout and field behavior,
  frecency/dir-first autocomplete via the shared popup, `$EDITOR` suspend/resume full
  repaint, streamed AI text into the real buffer with ai-tint-until-edit, revision cycling
  with write-back, Guardian BLOCK/WARN/NOTICE gating including the `:send!` bypass, full
  send-plan including undo-window lengthen-only and cursor-intact reopen, narrow-terminal
  Guardian fold)

## 150. First-run screen
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - Full-frame welcome exactly per §15.1, shown when `AccountService.List` returns empty;
    daemon connectivity shown inline. Four options (`a` add-from-email, `o` OAuth, `e` edit
    `rmail.toml`, `t` trainer) each individually reachable and tested.
  - `a` flow: email → `Autoconfigure` spinner with its exact copy → discovered servers +
    source badge + warnings + ready-to-paste TOML (verbatim, **nothing stored until
    confirmed** — a test asserts no `Account.Create` call occurs before the explicit
    confirm step) → credential step (keychain/command/env/OAuth via `BeginOAuth` with a
    cancel affordance while "waiting for browser consent…") → `TestConnection ✓` → budget
    prompt (**`$0` is accepted as a valid, complete answer** — a test specifically submits
    `$0` and asserts the flow proceeds rather than treating it as an empty/invalid field) →
    the Mail frame with initial sync running.
  - Status bar pins the hint line (`j/k move · e archive · ? help`) for the first 20
    actions of the session, then stops — counted, not time-based (a test performs exactly
    20 actions and asserts the hint is gone on the 21st).
- **verify:** `cargo nextest run -p rmail-cli tui::model` (empty-account-list triggers the
  screen, all four entry options, the `a` flow's nothing-stored-before-confirm property,
  `$0` budget accepted, 20-action hint countdown)

## 151. Trainer v2
- [ ] status
- **depends-on:** 133
- **parallel-safe:** no
- **acceptance:**
  - Full-frame app owned entirely by the TUI; renders its **own** practice rows inside its
    **own** widget, banner `TRAINER — practice rows, not your mail` — a test asserts no
    trainer row ever appears in `Model`'s real mailbox state (law-adjacent: "no
    client-invented mail", §20.9) — practice rows live in trainer-local state that the real
    List/Reader collections never read.
  - Ten rows, each naming the key that dismisses it; performing the named action animates
    the row out and advances; the sequence matches §15.2's exact key list (`j/k → Enter →
    e → d/u → x x d → f → / → 'u → Z → ?`); clearing all rows renders the "first earned
    zero state" (reusing whatever empty-state component task 156 establishes, not a
    trainer-specific one).
  - Reachable via `t` on first-run (task 150) and `Space h t` from anywhere, at any time,
    not just once.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (practice-row isolation from real
  mailbox state, all ten steps individually advance on their named key and no other,
  zero-state on completion reuses the shared empty-state component, reachable both from
  first-run and `Space h t`)

## 152. Initial sync screen honesty badges
- [ ] status
- **depends-on:** 120, 123
- **parallel-safe:** no
- **acceptance:**
  - Rows appear as headers land via `WatchEvents` during initial sync, skeletons (task 144)
    below the loaded rows, exactly matching §15.3's layout — the user can triage immediately
    on what's loaded rather than waiting for sync to finish (a test asserts list-card verbs
    work on the partially-synced rows with no special-casing).
  - Sync/index progress gauges render honestly: `SYNC ⠧ 2.4k/min`, `IDX ⠋ 3%`, `AI – (no
    budget)` when unset — each sourced from the real `Sync.Status`/`Index.Status`/
    `Ai.GetUsage` fields, never a client-estimated percentage dressed up as the daemon's.
  - `/` during initial sync shows the `indexing… lexical fallback` degradation badge
    (reusing task 140's exact badge mechanism, not a sync-specific copy of it).
- **verify:** `cargo nextest run -p rmail-cli tui::view` (rows-plus-skeleton composition
  during a simulated initial sync, gauge values traced to real proto fields with no
  client-side estimation, search degradation badge fires during this state via the shared
  mechanism)

## 153. Offline banner
- [ ] status
- **depends-on:** 120
- **parallel-safe:** no
- **acceptance:**
  - Populates the reserved banner row (task 120) amber with §15.5's exact content:
    `▲ offline since HH:MM — retrying ⠙ Ns · queued: N sends · N flag changes (all
    durable) · reading, search, tags, notes, local-AI all work`.
  - Queued mutations carry a `⇡` glyph in the list until reconciled; late sends get a
    `sent late` marker — both sourced from the outbox/mutation-queue state that already
    exists (task 157 gives outbox its full collection; this task only needs the glyph
    contract, which can be built against that task's minimal interface).
  - `Space d s` forces a retry from anywhere while the banner is up (not only from a
    dedicated screen).
- **verify:** `cargo nextest run -p rmail-cli tui::view` (banner content and amber tone
  under a simulated IMAP-down/daemon-up state, `⇡` glyph and `sent late` marker on queued/
  late rows, `Space d s` forces retry)

## 154. Daemon-down screen
- [ ] status
- **depends-on:** none
- **parallel-safe:** yes
- **acceptance:**
  - Full-frame screen exactly per §15.6, shown when the daemon socket is unreachable; last-
    known data stays behind it, marked stale (not discarded — a test asserts `Model`'s
    cached rows survive a disconnect and are still present, dimmed/stale-flagged, when the
    screen is dismissed after reconnect).
  - `!` **actually spawns `rmaild`** (the §1.2-mandated graft) and auto-reattaches on
    success — a test with a fake process spawner asserts the exact command invoked and
    that a successful spawn transitions the model out of this screen without a manual `r`.
  - `r` retries now; `l` tails the last 50 lines of the daemon log; `y` copies the
    launchctl start command (OSC 52 + arboard, matching the yank chord's copy mechanism);
    `q` quits.
  - Reconnect resumes `WatchEvents` from the stored seq — **no missed events** (reuses
    task 147's exact resume mechanism; this screen is a consumer of it, not a second
    implementation) — the screen's own text names the resume point honestly (the actual
    stored `since_seq`, not a placeholder).
- **verify:** `cargo nextest run -p rmail-cli tui::model` (stale-not-discarded cached data,
  `!` spawn command + auto-reattach via a fake spawner, `r`/`l`/`y`/`q` each individually,
  reconnect resumes from the real stored seq with zero missed events in a fixture replay)

## 155. Auth states at startup
- [ ] status
- **depends-on:** none
- **parallel-safe:** yes
- **acceptance:**
  - `AuthStatus.local_login_required` shows a one-field password screen before the frame
    (`LoginPassword`); the bearer token is held **only** in memory — a test asserts it is
    never written to `tui.toml`, history, or any file this task's diff touches (a grep-
    style test over the write paths, not just "we didn't add a write call").
  - `RESOURCE_EXHAUSTED` lockout shows the same screen with a live countdown
    (`locked — try again in Ns`) and the retry control genuinely disabled (not merely
    visually greyed — a test attempts submit during the lockout window and asserts no
    `LoginPassword` RPC is issued) until it elapses, at which point it re-enables itself
    without requiring a keypress to notice.
- **verify:** `cargo nextest run -p rmail-cli tui::model` (login-required gate precedes the
  frame, bearer token never persisted to disk, lockout countdown blocks the RPC call
  during its window and self-clears at zero)

## 156. Empty/edge states
- [ ] status
- **depends-on:** 123, 129
- **parallel-safe:** no
- **acceptance:**
  - One shared empty-state component (reused by trainer's zero-state, task 151, and every
    collection's empty rendering) covers: empty folder (`∅ nothing here · < > other
    lenses · C-p jump anywhere`); empty search (`0 hits for "…" · ⏎ try ~semantic · e
    edit query · sources weak: …` with the live semantic-index-build percentage, not a
    static claim).
  - Huge-mailbox honesty: title is the tell (`Archive ↓date [500/812,440 ·⠿]`); `G` pages
    on; filter/sort annotate `(partial)`/`loaded`; a one-time hint (not repeated every
    frame) names search as the full-corpus path — tested that the hint appears once per
    session, not on every render of a huge collection.
  - Error rendering: failed RPC → status message zone shows the exact format
    (`✗ Move failed: PERMISSION_DENIED (token lacks mail.write) — :token list`), sticky
    until a keypress, **also** appended to the notification feed (re-confirms task 129's
    dual-effect contract in this specific error-copy context); the optimistic change rolls
    back visibly (re-confirms task 133's rollback contract). Parse errors in prompts render
    red inline; the overlay stays open (re-confirms task 141's stays-open contract for
    prompts generally, not just filter).
- **verify:** `cargo nextest run -p rmail-cli tui::view` (shared empty-state component
  instantiated identically across trainer/folder/search, huge-mailbox title-tell and
  one-time hint, error message exact format + dual-effect + visible rollback, prompt
  parse-error stays-open)

## 157. Outbox collection
- [ ] status
- **depends-on:** 119, 112
- **parallel-safe:** yes
- **acceptance:**
  - `go` registry entry (task 119) backed by `ListOutbox` + live `WatchOutbox` — real data
    replacing task 119's minimal stub.
  - Rows render exactly per §16.1: state glyph (`◷ scheduled · ⧗ sending · ✓ sent · ✗
    failed · · canceled · ? uncertain`), to, subject, when (+`optimal ✦` marker for
    `SuggestSendTime`-sourced schedules), origin (`«ai»` rows always show an undo window —
    a test asserts an AI-originated row's undo window is never absent, even if the general
    undo policy would otherwise omit one for a short window), attempts, `last_error`
    verbatim (not summarized/truncated).
  - Verbs per the arbitration table (task 135): Enter inspect, `e` edit body (draft-
    backed, reuses task 149's compose), `s` send-now, `u` cancel (task 112's stack), `R`
    retry, `t` reschedule via task 138's time-input popup.
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (row rendering for every
  state glyph, AI-origin undo-window guarantee, all six verbs dispatch correctly including
  the compose/time-popup integrations)

## 158. Follow-ups + Waiting-on collections
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - `gf` (Follow-ups): armed/fired state, remind-at, note-to-self banner on resurface; `d`
    dismiss, `b` new (`CreateFollowup`).
  - `gw` (Waiting-on): longest-wait-first ordering **taken verbatim from wire order**
    (§16.2 — a test asserts the client does not re-sort what the server already ordered),
    overdue rows red with age shown, `ask` column names "the one thing being waited on"
    (not a generic subject line); `N` drafts a nudge via `DraftNudge` → composer (task 149)
    pre-filled.
  - Sidebar badges (task 122's QUEUES section) read `3` / `5·2!` matching these
    collections' live counts exactly — a test changes the underlying data and asserts the
    sidebar badge updates without a manual refresh.
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (Follow-ups armed/fired/
  dismiss/new, Waiting-on wire-order-preserved + overdue styling + `ask` column + nudge
  draft prefill, sidebar badge live-sync for both)

## 159. Notifications collection
- [ ] status
- **depends-on:** 119, 131
- **parallel-safe:** yes
- **acceptance:**
  - `gn` registry entry backed by the exact feed task 131 already built (durable,
    `StreamAlerts since_id`, merged local history) — this task is the collection-façade
    over that feed, not a second feed implementation.
  - Tier-colored rows with `«reason»`; Enter opens the message; `w` explains via
    `ScoreMessage` showing threshold/suppression/would-notify verdict — a test asserts all
    three verdict fields render, not just a boolean fired/didn't-fire.
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (façade reuses task 131's
  feed with no duplicated stream state, tier coloring, Enter-opens, `w` three-field verdict)

## 160. Jobs collection
- [ ] status
- **depends-on:** 119, 131
- **parallel-safe:** yes
- **acceptance:**
  - `gj` registry entry backed by the jobs feed already built in task 131 — same
    façade-not-duplicate relationship as task 159.
  - Cancel key reaches the real cancellation path for each job kind (export, attachment
    save, reindex drain, bulk action) — a test per kind asserts the underlying stream
    actually receives a cancellation, not just that the row's UI state changes locally.
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (façade reuse, per-kind
  cancel reaches the real stream cancellation)

## 161. Insights — Analytics
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - `gd` hub's Analytics surface: response-time p50/p90 you-vs-them, weekly trend
    sparkline, attention-first group table with bottleneck/stalled chips, all from
    `GetResponseTimes`.
  - `q` NL analytics via `AskAnalytics`: answer renders as rows + `«narrative»` **with the
    sandboxed SQL always shown** — a test asserts the SQL is present in every render of an
    `AskAnalytics` result, not collapsible-to-absent (§16.5's checkability requirement is
    non-negotiable, not a toggle).
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (response-time table and
  sparkline from real field data, `AskAnalytics` narrative+rows+always-visible-SQL)

## 162. Insights — Contact page
- [ ] status
- **depends-on:** 119, 128
- **parallel-safe:** yes
- **acceptance:**
  - Full contact page (`gc` from any message, and the rail Contact tab's Enter from task
    128) beyond the rail's metrics-only summary: volume, symmetry, cadence, decay, topics,
    `«briefing»`, ≤5 next actions each Enter-able — a test asserts the action list is
    capped at 5 even when the backend would return more (the cap is a client-side
    presentation decision the acceptance fixes, not left to whatever the RPC returns).
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (full card content, 5-action
  cap, each action Enter-able and dispatches correctly, rail-Contact-tab Enter reaches this
  page)

## 163. Insights — Subscriptions
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - `gv`: sender, class + source badge (`HEADER`/`HEURISTIC`/`«MODEL»` — the badge
    literally names which of the three produced the classification, per §16.5), read-rate
    meter, cadence, expandable signals.
  - `U` shows the unsubscribe **proposal** (http/mailto/one-click) — **rmail never
    unsubscribes itself**, enforced as a test that no code path in this task issues an
    outbound unsubscribe request; `y` copies the link, Enter opens the browser. This is the
    same "report, never repair" pattern as the deceptive-link handling in task 127 — worth
    keeping consistent, not a separate design.
  - `classify_unknown` is a labeled `$` action (costs money, says so before running).
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (source-badge accuracy per
  classification origin, no self-unsubscribe code path exists, `y`/Enter on the proposal,
  `$` label on classify_unknown)

## 164. Insights — Invoices
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - `gi`: vendor/number/total/due/status columns, **every cell provenance-tagged**
    (`parsed` plain vs. `«model»`) — a test asserts no cell in this collection is ever
    rendered without one of the two provenance markers, closing off the "which numbers can
    I trust" ambiguity by construction.
  - `e` CSV export (`ExportInvoices`, jobs-feed progress via task 131/160's mechanism);
    Enter opens the source message (task 119's detail-renderer contract).
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (provenance tag on every
  cell — a fixture with mixed parsed/model cells specifically, `e` export reaches jobs
  feed, Enter opens source)

## 165. Insights — Digest renderer
- [ ] status
- **depends-on:** 127, 119
- **parallel-safe:** yes
- **acceptance:**
  - Markdown digest via the Reader's shared body renderer (task 127's measure-clamp
    function, reused — not a fifth wrapping implementation); every line's `[msg:id]`
    citation is Enter-able and opens the cited message.
  - `]`/`[` walk sections; `r` regenerates (force, labeled `$` since it's a model call);
    `cached` badge when serving `GenerateDigest`'s cached result rather than regenerating.
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (shared measure-clamp reuse
  confirmed via the same test helper task 127 introduced, citation Enter-navigation,
  section walk, `r`/`$`/`cached` badge states)

## 166. Automation — Rules
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - `gr` Rules surface: table + TOML detail (task 119's detail-renderer contract, reusing
    the existing `ConfigBlock`/report machinery rather than a new TOML viewer); `Space`
    toggles enabled in place (optimistic, task 133's pattern).
  - `n` NL synthesis: instruction → generated TOML + 30-day dry-run hits + stats → explicit
    confirm to store — a test asserts nothing is persisted before the confirm step (same
    "nothing stored until confirmed" property as task 150's account flow, now proven here
    too since it's the same class of "model proposes, user confirms" interaction).
  - `B` backtest: per-predicate outcome table, model-call/cache stats, `«explanation»` per
    `claude_is` decision; corrections recorded from mis-fires feed back into the rule
    (reusing whatever correction-recording mechanism v1's rule work already established —
    verified present before assuming it, not reinvented).
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (table+TOML detail, `Space`
  optimistic toggle, NL-synthesis nothing-stored-before-confirm, backtest table content and
  correction feedback path)

## 167. Automation — Agent
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - `RunInboxAgent` is **dry-run by default** — a test asserts the default invocation path
    never mutates mail state, only produces the action table with `«reason»` and outcome.
  - Mutating runs are gated behind explicit scopes **and** a typed confirmation (not a
    `y/n`, matching §8.2's "type-the-name for nuclear ops" pattern extended here since an
    agent acting autonomously on mail is at least as consequential as deleting an account).
  - Run history is browsable as a collection (task 119) showing past runs' action tables.
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (dry-run-by-default proven
  by zero mutating calls in the default path, scope-gate + typed-confirm required for a
  mutating run, run history browsable)

## 168. Automation — Hooks + Webhooks
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - Hooks: list + `t` test showing exit code and stdout/stderr (reusing the existing hook
    test-run RPC, not a client-side re-implementation of hook execution).
  - Webhooks: destinations with URLs **redacted to authority** by default (e.g.
    `https://hooks.example.com/…` not the full path/query, which may carry a secret token)
    — `reveal` is an explicit action, not the default rendering; deliveries sub-list with
    `replay`.
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (hook test-run shows real
  exit/stdout/stderr, webhook URL redacted-by-default with explicit reveal, delivery replay
  reaches the real RPC)

## 169. Settings v2
- [ ] status
- **depends-on:** 114, 117
- **parallel-safe:** no
- **acceptance:**
  - Full-frame app, all 14 sections from the existing v1 settings screen (task 101) carried
    forward, now with **current values inline** wherever a read RPC or local state exists:
    spend via `GetSpend`, provider chain via `GetAiProvider` rendered as
    configured→override→effective + policy mode, keymap, theme/density (task 114's prefs) —
    a test enumerates all 14 sections and asserts every field that has a corresponding read
    RPC or prefs value shows it, not a blank/placeholder.
  - Config-file-only keys render via the existing `ConfigBlock` (path + effect timing +
    open-to-copy) — reusing task 96/101's existing mechanism verbatim, not rebuilt.
  - Keys section is the conflict-lint view (task 139's shadow lint) + `c` rebind (task
    139's flow) — this task is the settings-screen *presentation* of machinery task 139
    already built, proven by a test that no rebind logic is duplicated here.
- **verify:** `cargo nextest run -p rmail-cli tui::settings` (all 14 sections present with
  live values where a source exists, `ConfigBlock` reuse for file-only keys, Keys section
  delegates to task 139's lint/rebind without duplication)

## 170. Tokens/audit v2
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - Token table (scopes, last used, revoke) + mint flow where the secret is shown exactly
    once and a re-run of the same mint request is refused (idempotency at the UI level
    matching the RPC's own semantics — a test mints twice with the same client-generated
    key and asserts the second call surfaces the "already minted, not shown again" refusal
    rather than silently re-displaying the secret).
  - Audit ledger: `QueryAiCalls` paginated (model, pass, tokens, cost, redaction level,
    latency, status) with a filter row; `ExportLedger` streams to a file with jobs-feed
    progress (task 131/160's mechanism).
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (token table+mint-once-only
  semantics, audit ledger pagination+filter, export reaches jobs feed)

## 171. Ask (RAG)
- [ ] status
- **depends-on:** 128
- **parallel-safe:** no
- **acceptance:**
  - Rail Ask tab (`A`/`ga`; zoom or narrow width promotes to full-frame via task 109's zoom
    mechanism) renders the **fixed frame order** honestly per §16.9: retrieval trace line
    (`retrieved 24 · packed 9 · withheld 2 by policy`) → streamed tokens (64 KiB cap, marked
    truncation when hit) → citations (`[n]` aligned with inline markers) → the daemon's
    `grounded`/refusal verdict rendered as the **daemon's claim**, not the client's — a test
    asserts the verdict text is sourced from the RPC response field, never inferred
    client-side from the presence/absence of citations.
  - **Citations are verbatim mailbox facts, never `«»`-marked** — the one deliberate
    exception to law 9, and a test specifically asserts citation spans do NOT carry the
    `ai` tint/guillemets that surrounding narrative text does, since marking a verbatim
    quote as "model-generated" would be its own honesty violation.
  - Ask-attachment (`?` in the attachment browser, task 127) reuses this exact surface with
    page/span citations instead of message citations — proven by a shared rendering
    function, not a parallel implementation.
  - Truncation at the 64 KiB cap is visibly marked, never silent.
- **verify:** `cargo nextest run -p rmail-cli tui::view` (fixed frame order including
  withheld-by-policy count, citation-text lacks ai-tint/guillemets while narrative text has
  them, daemon-sourced verdict field, Ask-attachment shares the render function, truncation
  marker at the cap boundary)

## 172. Help & manual v2
- [ ] status
- **depends-on:** 136
- **parallel-safe:** no
- **acceptance:**
  - `?` mode-aware searchable overlay (task 108's stack), grouped by verb path; Enter runs,
    `c` rebinds (task 139's flow), `K` jumps to the manual page — all three reusing existing
    mechanisms, this task's job is the mode-aware grouping and search over the v2 action set.
  - `gh` Manual: full-frame reader using task 127's shared measure-clamp; in-page find; `g/`
    grep-all-pages (reusing the existing `manual::grep` pure function from task 103/105,
    extended to cover every new manual page this part's tasks add); jump list `C-o`/`C-i`
    navigating manual history (a test walks forward/back through at least 3 visited pages
    and asserts the jump list ordering matches a browser-history model, not a stack that
    loses the forward direction after a new jump).
  - Trainer reachable via `Space h t` (task 151's binding, confirmed wired from the help
    surface too, not only first-run).
- **verify:** `cargo nextest run -p rmail-cli tui::help` · `cargo nextest run -p rmail-cli
  tui::manual` (mode-aware grouped search over the v2 action set, `c`/`K` delegation without
  duplication, measure-clamp reuse, grep-all-pages coverage of new pages, jump-list
  browser-history semantics)

## 173. Multi-account & unified inbox
- [ ] status
- **depends-on:** 119, 122
- **parallel-safe:** yes
- **acceptance:**
  - `g1`..`g9` switch the active account; `gu` opens Unified (`ListUnified` via task 119's
    registry) with a 1-column account-accent gutter on every row, sourced from `acct1..6`
    (task 117).
  - Every action taken on a unified row routes via **that row's own** real account/mailbox
    ids — a test constructs a unified view mixing two accounts and asserts an archive on a
    row from account 2 issues the RPC against account 2's mailbox, not account 1's or a
    default (the exact bug class this feature invites if the row's origin is lost anywhere
    in the dispatch path).
- **verify:** `cargo nextest run -p rmail-cli tui::collection` (g1..g9 switch, unified
  gutter coloring matches `acct1..6`, per-row action routes to the row's own real
  account/mailbox in a mixed-account fixture)

## 174. Quick menu `.` v2
- [ ] status
- **depends-on:** 119
- **parallel-safe:** yes
- **acceptance:**
  - For the cursored message: Summarize (cached, free — a test asserts zero model calls
    since it's reading the same `<5ms` cache as task 126's AI capsule), Ask (pre-filled
    into task 171's surface), Suggest reply/tags (labeled `$`), Extract
    (events/tasks/invoice/links), each dispatching to its real existing RPC.
  - Mute-rule proposal opens `:rule new` pre-filled — **honest**: there is no
    `MuteService` (§19.5), and a test asserts no code path in this menu claims to mute
    anything directly; the affordance's own label says "propose a rule", not "mute".
- **verify:** `cargo nextest run -p rmail-cli tui::overlays` (each action dispatches to its
  real RPC with Summarize proven cache-only, mute proposal opens rule-synthesis pre-filled
  with no direct-mute code path anywhere)

## 175. Daemon-gap degradation badges
- [ ] status
- **depends-on:** 111, 119, 140
- **parallel-safe:** no
- **acceptance:**
  - All thirteen §19 gaps are checked, one by one, against the shipped surfaces from tasks
    120–174, and each is confirmed to render its documented honest label rather than a
    faked value: folder unread (`~`/`•`, task 111) · lens/search counts (task 113's honesty
    machine) · thread-per-row folders (flat + `⧉N` + `pt`, task 124/134) · snooze-is-really-
    a-followup (`b`'s copy, task 133) · mute-is-really-a-rule-proposal (task 174) · no NL
    on `:` (the command line stays deterministic — a test asserts no free-text fallback
    exists on `:`) · no screener storage (lenses stay client-side approximations, task 113)
    · no redaction-preview RPC / no `:ai policy explain` (Settings, task 169, shows policy
    from `GetAiProvider` only, nothing further claimed) · `AccountService.Update` is
    delete+create (Settings says so explicitly) · archive-is-a-heuristic-Move (documented
    at the point `e` is bound, task 133) · folder sort is loaded-rows-only (task 143's
    `(sorts N loaded)` string) · the four services with no backend surface at all (Prompt
    library/Conversation memory/bulk-undo/ghost-text) have **no UI entry point whatsoever**
    — a test greps the whole `tui/` tree for any reference to these four and asserts none
    exists, since a reserved-but-dead menu item would itself be the "invisible state" law
    violation · encryption/signing glyph position reserved but not rendered until the wire
    type carries the field (a test asserts the glyph cell renders empty, not a fake
    "unencrypted" claim, when the field is absent).
  - This task adds **zero** new RPCs — its only job is auditing that every gap is labeled
    where §19 says it should be, and fixing any surface from 120–174 that was found to
    silently paper over one instead.
- **verify:** `cargo nextest run -p rmail-cli tui::` (one test per §19 gap asserting its
  honest-label rendering at the specific surface that could otherwise fake it, plus the
  two dead-feature absence greps)

## 176. Re-homed keys migration
- [ ] status
- **depends-on:** 139, 172
- **parallel-safe:** no
- **acceptance:**
  - Every re-homing §21 documents from the *current* (Part V) build to this design is
    applied: `a` archive→`e`, `s` toggle-read→`U`, `f` flag→`s`, `c` copy-to→`M`, `M`
    move-to→`m`, `o` open-html→`H`, `x` explain→`w`, `O` outbox→`go`; `gs` settings
    unchanged. A test walks this exact old→new table and asserts every new binding is live
    and — critically — that **no old binding silently still works with its old meaning**
    (the specific failure mode a half-finished migration produces: two keys doing the same
    thing, or one key doing something different than the manual now says).
  - A new manual page (extending task 172's manual) documents the full old→new table for
    anyone whose personal `keys.toml` still binds a pre-migration key — reusing task 139's
    shadow-lint mechanism to detect exactly that case and point at this page by name in the
    lint message, not just a generic "shadowed" warning.
- **verify:** `cargo nextest run -p rmail-core keymap::` (every re-homed key resolves to
  its documented new `Action` and the old meaning is unreachable) · `cargo nextest run -p
  rmail-cli tui::manual` (migration page present and linked from the shadow-lint message)

## 177. Appendix A width/height matrix golden-frame suite
- [ ] status
- **depends-on:** 107, 123, 126, 129, 130
- **parallel-safe:** yes
- **acceptance:**
  - The view test suite renders every frame at the exact matrix Appendix A declares
    normative — widths `{80, 100, 120, 160, 200}` × heights `{24, 30, 50}` — and asserts,
    per §18's binding checklist: no `Rect` overflow anywhere in the tree, every column-
    budget sum from task 123's per-breakpoint table ≤ inner width at that exact size, and
    the drop orders from §4.3/§4.3's three named tables fire at exactly their documented
    thresholds (not one column early or late).
  - Appendix A.1–A.4's four reference frames (wide XL cozy, narrow S-stacked, zoomed
    Reader, zoomed List triage table) are each reproduced as a golden test at their stated
    dimensions and diffed structurally (card boundaries, zone presence/absence, title-line
    content) — not a brittle byte-for-byte terminal-output comparison, which would break on
    every cosmetic change, but a structural assertion of what §18 actually commits to.
  - This suite is the **normative artifact** §18 names — a task anywhere in 120–176 whose
    change breaks a case here is a regression in that task, not in this one; this task's
    job is that the suite exists and is comprehensive, not that everything upstream already
    passes it (upstream tasks are expected to keep it green as they land, per this part's
    cross-cutting acceptance).
- **verify:** `cargo nextest run -p rmail-cli tui::view` (the full 15-point width×height
  matrix with zero overflow and correct budget sums at every point, all four Appendix A
  reference frames structurally golden-tested, every documented drop-order threshold exact)

## 178. Performance budget verification
- [ ] status
- **depends-on:** 177
- **parallel-safe:** yes
- **acceptance:**
  - Each §18 budget gets an instrumented check: TUI attach < 200 ms (first frame before any
    RPC returns — re-confirms the existing v1 invariant still holds with the v2 frame's
    larger initial render), first search hit < 30 ms end-to-end, finder first batch < 16
    ms, open message < 30 ms, AI panel read < 5 ms (the cache-only path from task 126),
    frame build+diff < 4 ms typical / 16 ms worst (task 148's instrumentation, now checked
    against a number), input-to-paint < 20 ms.
  - Where CI hardware makes a hard latency assertion flaky, the check runs as a `tracing`-
    logged measurement with a documented local-dev threshold rather than a hard failure —
    consistent with task 148's approach — but every budget is at minimum *measured and
    reported* on every run, so a regression is visible in the numbers even when it doesn't
    fail the build.
  - §18's 17-point craft-rules checklist is walked explicitly, one line per rule, each
    either citing the task that proves it or filing a gap if one is found — this is the
    closing audit, not new implementation.
- **verify:** `cargo nextest run -p rmail-cli tui::` (instrumented measurement present for
  all seven named budgets) · a written craft-rules audit (17 lines, one per §18 rule,
  each resolved to a task or a filed gap) attached to the commit body

## 179. Final cleanup and completion report
- [ ] status
- **depends-on:** 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120,
  121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137,
  138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154,
  155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171,
  172, 173, 174, 175, 176, 177, 178
- **parallel-safe:** no
- **acceptance:**
  - Every superseded v1 rendering path named in this part's preamble ("Supersedes, task by
    task") is confirmed unreachable from any live keybinding — the old three-screen
    `Screen::Viewer` path, the single-overlay-slot code, the headers-only preview, the
    bespoke AI-panel column, the `O`-outbox-overlay — each either deleted outright or, if
    the underlying function is still called by the new path, stripped of its own
    now-dead entry points. No `#[allow(dead_code)]` left over from the migration.
  - Full pipeline clean: `cargo fmt --all -- --check`, `cargo clippy --all-targets
    --all-features -- -D warnings`, `scripts/docker-test.sh` (whole workspace), `cargo
    build --release`, `cargo deny check`, `cargo audit`, `buf breaking` if `buf.yaml`
    resolves. Zero `unwrap()/expect()/panic!/todo!` in non-test code anywhere under
    `rmail-cli/src/tui/` (a workspace-wide grep gate, not a sample check).
  - A completion report is produced (not committed as a new doc unless asked — printed in
    the session and in the commit body): what shipped per phase, how to run it (`mail tui`
    against a real or trainer-backed daemon), test count added by this part vs. Part I–V's
    baseline, and an honest list of anything from `tui.md` that did **not** ship — if
    anything remains, it stays unchecked here with a note why, rather than this task
    checking itself off over a gap.
- **verify:** `cargo fmt --all -- --check` · `cargo clippy --all-targets --all-features --
  -D warnings` · `scripts/docker-test.sh` · `cargo build --release` · `cargo deny check` ·
  `cargo audit` · `buf breaking --against proto/buf-baseline.binpb` (if resolvable) ·
  `grep -rn "unwrap()\|expect(\|panic!\|todo!" rmail-cli/src/tui --include=*.rs | grep -v
  /tests` (must be empty)

## Where this leaves the plan

Parts I–V are checked off in full — the current build (tasks 1–106) is a complete,
production TUI over a complete backend. Part VI is `tui.md`'s redesign of that same TUI's
view and interaction layer, decomposed above into 73 dependency-ordered tasks (107–179)
that a fresh `/loop` resumes from the first unchecked one. No task in Part VI adds an RPC
— every wire call `tui.md` names was verified against `proto/` before this file was
written — so Part VI is purely `rmail-cli/src/tui/**` plus the small `rmail-core::keymap`
extensions the new chords/actions need. When 179 is checked, re-read this section: it
should say all 179 are done, or name exactly what's left and why.
