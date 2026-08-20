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
- [ ] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - `AiPolicyService` (4), `AiSafetyService` (2), and `AuditService` (2) wired behind `:ai budget status|set`, `:ai provider status|set`, `:ai scan`, `:ai audit`.
  - `:ai budget set` with no arguments opens the Settings-style form pre-filled from `GetSpend` rather than issuing a partial `SetBudget` (which would clear unset caps); flags pre-fill the form; a trailing `!` applies immediately with CLI replace-semantics. Spend renders against caps with soft/hard color *and* glyph.
- **verify:** `cargo nextest run -p rmail-cli tui::commands::ai_policy` (bare budget-set opens prefilled form, bang applies immediately and clears unset caps, soft/hard glyph thresholds)

## 97. Accounts, sync control and tokens
- [ ] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - `Model.accounts: Vec<Account>` added alongside the existing single `account` field; `:account use <id>` switches the active account within a session (previously explicitly deferred).
  - All nine `AccountService` RPCs and all three `AdminService` RPCs wired behind `:account list|show|add|login|refresh|test|rm|use` and `:token list|create|revoke`; `Autoconfigure` output and OAuth URLs render in a Report with copy/open affordances (reusing the existing `html::CommandOpener`); a minted token secret is shown exactly once with an unrecoverable marker.
- **verify:** `cargo nextest run -p rmail-cli tui::commands::account tui::commands::token` (in-session account switch, OAuth URL open path, token shown once and not recoverable from subsequent state)

## 98. Automation and notifications
- [ ] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - `WebhookService` (7), `HookService` (2), and `NotificationService` (2) wired behind `:webhook list|add|rm|enable|disable|deliveries|replay`, `:hook list|test|add`, `:forward`, `:notify list|score|set`.
  - `:hook add` and `:notify set` follow the `ReadOnlyReason::ConfigFileOnly` presentation established in task 101's field model — the exact TOML block to paste, its path, and a copy affordance — never a fabricated write RPC. `:notify list` renders `StreamAlerts` live in a Report.
- **verify:** `cargo nextest run -p rmail-cli tui::commands::automation` (webhook CRUD + replay, hook test round-trip, config-block presentation for hook/notify config-only fields, live alert Report)

## 99. Content, export and analytics commands
- [ ] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - `ExportService`, `AnalyticsService` (5), `AttachmentService` (4), `ExtractService` (3), `LinkService`, `NoteService` (5), `SavedSearchService` (11), and the untouched `SearchService` methods (`CompileQuery`, `SearchAttachments`, `SearchEntities`, `Evaluate`) wired behind `:export`, `:digest`, `:stats response-time|ask`, `:contact`, `:subs`, `:attach list|tables|invoice|ask|search`, `:extract events|tasks|data`, `:links`, `:note add|list|edit|rm`, `:saved list|save|run|rm`, `:folder new|list|members|eval|rm`, `:search compile|attachments|entities|eval`.
  - `:digest` rows open their cited source message on Enter.
- **verify:** `cargo nextest run -p rmail-cli tui::commands::content` (export streams to each format, digest row citation navigation, saved-search/smart-folder CRUD, attachment ask/search)

## 100. Compose, send and follow-up commands
- [ ] status
- **depends-on:** 94
- **parallel-safe:** yes
- **acceptance:**
  - All ten `ComposeService` RPCs and the remaining `SendSchedulerService` methods wired behind `:reply [--ai]` (streaming `DraftReply`), `:draft list|show|rewrite|revisions|revert`, `:send [--at][--undo]`, `:outbox`/`:outbox cancel|retry|reschedule`, `:followup list|new|dismiss`, `:waiting`, `:nudge`, `:preflight`.
  - The existing undo toast remains the only countdown surface (no second one introduced for the command path).
- **verify:** `cargo nextest run -p rmail-cli tui::commands::compose` (AI reply streams to an editable draft, send/undo window unchanged, follow-up lifecycle round-trips)

## 101. Settings screen
- [ ] status
- **depends-on:** 95, 96, 97, 98
- **parallel-safe:** no
- **acceptance:**
  - `Screen::Settings` and `Mode::Settings` (chain `[Settings, Global]`, restating j/k/gg/G/`<tab>`/`<enter>` rather than inheriting `Normal` — the same reason `Menu`/`Pick` already restate them). Reached via `:settings [<section>]`, the `<space>cc` leader chord (task 105), and an `s` key from any Report.
  - A `FieldKind` model (`Toggle`, `Choice`, `Number`, `Text`, `Run`, `ReadOnly{ConfigFileOnly|NoRpc}`) where **every field's write is expressed as a `:` command `Invocation`** — the screen has no private path to the daemon, so it is testable by asserting the invocation a keypress produces, with no daemon required. `ReadOnly::ConfigFileOnly` fields render the exact TOML block, its path, and a copy affordance. Settings › Keys writes through `rmail_core::keymap::file::edit` directly, not `ConfigService.SetBinding`, so rebinding still works with the daemon down.
  - Sections: Accounts, Sync, Index, AI, Safety & audit, Rules, Tags, Automation, Notifications, Saved searches, Keys, Interface, Tokens, Daemon.
- **verify:** `cargo nextest run -p rmail-cli tui::settings` (every field's keypress produces the expected Invocation with no daemon connection, config-file-only fields render their block, Keys section bypasses gRPC)

## 102. Help overlay redesign
- [ ] status
- **depends-on:** 91
- **parallel-safe:** yes
- **acceptance:**
  - `?` becomes mode-aware (renders `Model::mode()`'s actual chain at the moment it was pressed, with `<tab>` cycling to other modes), scrollable (no silent truncation past the terminal height), grouped by the same derived id-prefix grouping WhichKey uses, and filterable with `/` (reusing `palette_matches`' tiers over chord/id/description).
  - No longer a dead end: `<enter>` on a row runs the action, `c` opens `:keys set <chord> <action>` pre-filled, `K` navigates to that action's manual page (task 103).
- **verify:** `cargo nextest run -p rmail-cli tui::help` (mode-chain rendering per invoking mode, scroll past terminal height, filter tiers match palette's, row actions run/rebind/navigate correctly)

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
- [ ] status
- **depends-on:** 91, 95, 96, 97, 98, 99, 100
- **parallel-safe:** no
- **acceptance:**
  - `<space>` installed as a leader in Normal/Viewer/Visual with the default group map (`<space>a` ai, `<space>t` tag, `<space>r` rule, `<space>d` daemon, `<space>c` config/settings, `<space>s` search/saved, `<space>o` outbox/send, `<space>x` extract/attach, `<space>n` note, `<space>g` goto, `<space>w` webhook/hook, `<space>h` help) — every group label still derived (task 91), not hand-written.
  - `Key` extended with `Left`/`Right`/`Home`/`End`/`PageUp`/`PageDown`, including `named_key` spellings and the crossterm-to-`Key` conversion, which silently drops them today.
  - `Keymap::shadowed_across_layers` (task 91) runs as a startup lint printing a warning for any hit, and is reachable as `:keys check`.
  - No default binding already shipped is removed or rebound; `palette` remains a working alias of `command`; a migration note in the manual (task 104) covers anyone whose own `keys.toml` already binds `:` or `<space>`.
- **verify:** `cargo nextest run -p rmail-core keymap::` · `cargo nextest run -p rmail-cli tui::model` (leader chords resolve to the right groups, new `Key` variants round-trip through parse/display, startup shadow-lint fires, no regression in existing default bindings)
- **verify:** `cargo deny check` · `cargo audit` · `buf breaking --against proto/buf-baseline.binpb` (this repo has no `main` branch or remote for `.git#branch=main` to resolve against — see `scripts/update-buf-baseline.sh`) · `cargo bench -p rmail-core --no-run`
