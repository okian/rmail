# Product Requirements Document (PRD)

## Project

**rmail — Local-first CLI/TUI IMAP Mail Client and Mail↔AI Bridge (MCP + gRPC)**

Version: 0.2

---

# Vision

Build a fast, local-first email client for macOS that behaves more like `git`, `ripgrep`, or `k9s` than a traditional GUI mail client.

The application continuously synchronizes IMAP accounts in the background, stores all email locally, deeply indexes everything, and exposes **every** capability to both humans (CLI/TUI) and AI agents (MCP + a complete gRPC API).

The overriding goal of the project is to be a **full-featured bridge between email and AI — primarily Anthropic's Claude.** Mail becomes an addressable, queryable, AI-native corpus:

- Every message is summarized, triaged, tagged, and embedded on arrival.
- Every question can be answered over the local mailbox with grounded citations.
- Every action a human can take, an agent can take too — under least-privilege policy.

Primary goals:

- **Relevance** — the right message is in the top results, always. Search is the product.
- Fast
- Reliable
- Local-first
- Scriptable
- AI-friendly

Non-goals:

- Competing with Apple Mail or Thunderbird on rich HTML rendering
- Calendar as a first-class app (we extract events, we don't render a calendar)
- Contacts management UI (we derive a contact graph, not a CRM)
- Exchange support (initially)

---

# The Mail ↔ AI Bridge (North Star)

rmail is not "a mail client that has some AI features." It is a **bridge**: one core engine over the local mailbox, fronted by three thin adapters — CLI, TUI, and an API layer (gRPC + MCP).

```
                 ┌───────────────────────────────┐
   IMAP  ───────▶│            rmaild             │
   SMTP  ◀───────│   (daemon: sync + indexing +  │
                 │    ranking + AI + gRPC server)│
                 │                               │
                 │   ┌──────────────────────┐    │
                 │   │  Core Services (Rust) │    │
                 │   └──────────▲───────────┘    │
                 │              │ one core API    │
                 └──────────────┼─────────────────┘
                                │
        ┌──────────┬────────────┼────────────┬──────────────┐
        │          │            │            │              │
      CLI        TUI       MCP server    gRPC-web      3rd-party
    (client)  (client)   (Claude tools) (browser)    scripts/agents
```

**Design invariant:** *If the CLI can do it, gRPC can do it. If gRPC can't do it, it isn't a feature. If gRPC can do it, Claude can do it (via MCP auto-projection).* There is never CLI/gRPC/MCP feature drift.

Claude is the default AI provider (`claude-opus-4-8` for depth, `claude-sonnet-5` balanced, `claude-haiku-4-5` for cheap high-volume triage), via the Anthropic Messages API — with a pluggable provider trait and a fully-local model/embedding path for privacy-sensitive accounts.

---

# Target Users

### Primary

Developers who use the terminal daily, have large mailboxes, value keyboard navigation, and use AI assistants.

### Secondary

Power users and agent-builders who want instant relevance-ranked search, offline access, automation, and a programmable mail backend for their own AI agents.

---

# Core Principles

## Relevance-First

Search is judged on one thing: **is the message I want in the top 3?** Every retrieval technique we know is combined into a single ranking cascade (see Part I). Nothing else in the product matters if search is mediocre.

## Local First

Every email exists locally. Searching, ranking, and reading never require contacting IMAP. The UI continues functioning offline. AI enrichment is cached forever.

## Background Sync

Synchronization, indexing, and AI enrichment happen independently from the UI in the `rmaild` daemon. The TUI never blocks on network or model calls.

## Keyboard First

Everything is accessible without a mouse. Inspired by mutt, lazygit, tig, k9s.

## API-First (gRPC + MCP)

All functionality is exposed through one core API surfaced over gRPC and MCP. The CLI/TUI are clients of it.

## AI-Native

AI is a first-class citizen, not a bolt-on: summaries, triage, tags, semantic recall, reranking, drafting, and Q&A are wired into the core data model and pipelines — always cost-bounded, redacted, and auditable.

## Extensible

Rules, hooks, webhooks, a prompt library, and generated client SDKs let users and agents extend behavior without forking.

---

# Platforms

Initial release: macOS. Future: Linux.

---
---

# PART I — THE RETRIEVAL & RANKING PIPELINE (Crown Jewel)

> This is the most important subsystem in rmail. It is designed to a higher standard than everything else. The target: **the message the user wants is in the top 3 results, on the first keystroke, offline, in under 150 ms.**

## Goal & Principles

- **Recall then precision.** Cast a wide net with many cheap retrievers, then spend compute narrowing to a precise, well-ordered head.
- **Combine every technique.** Lexical, dense-vector, fuzzy, entity, and structured retrieval each catch queries the others miss. Fuse them.
- **Learn from behavior.** What the user opens, replies to, and dwells on trains the ranker. Relevance improves with use.
- **Explainable.** Every result can answer "why did this rank here?" — for the user and for Claude.
- **Graceful degradation.** No embeddings? Lexical still ranks well. No model? A deterministic hand-tuned scorer runs. Offline? Everything works.
- **Streaming.** Results render best-first as they are produced; the UI never waits for the full set.

## The Cascade (multi-stage retrieval → ranking)

```
                                  Query
                                    │
                    ┌───────────────▼────────────────┐
                    │ [0] QUERY UNDERSTANDING          │
                    │  parse operators • detect intent │
                    │  spell-fix • expand • embed •    │
                    │  NL→plan (Claude, cached)        │
                    └───────────────┬──────────────────┘
                                    │  QueryPlan (terms, filters, vectors, intent)
     ┌──────────────┬──────────────┼───────────────┬───────────────┬─────────────┐
     ▼              ▼              ▼               ▼               ▼             ▼
[1] LEXICAL     DENSE VEC       FUZZY          ENTITY        STRUCTURED     RECENCY
   BM25/FTS5    kNN(sqlite-vec) nucleo/trigram  index         filter        prior
   field-weighted paraphrase    typo/partial   people/ids     from:/has:/    recent
   phrase/prox   recall          subj/sender    amounts/track  before: (hard) mail
     │              │              │               │               │             │
     └──────────────┴──────────────┼───────────────┴───────────────┴─────────────┘
                                    ▼
                    ┌───────────────────────────────────┐
                    │ [2] FUSION & DEDUP                 │
                    │  weighted Reciprocal Rank Fusion   │
                    │  chunks→message, (opt) msg→thread  │
                    └───────────────┬───────────────────┘
                                    │  ~200–500 fused candidates
                    ┌───────────────▼───────────────────┐
                    │ [3] FEATURE EXTRACTION             │
                    │  build a feature vector / candidate │
                    └───────────────┬───────────────────┘
                    ┌───────────────▼───────────────────┐
                    │ [4] L1 RANKER (fast, learned)      │
                    │  GBDT / linear over features;      │
                    │  scores all candidates             │
                    └───────────────┬───────────────────┘
                                    │  top-K (≈50)
                    ┌───────────────▼───────────────────┐
                    │ [5] L2 RERANKER (expensive, top-K) │
                    │  cross-encoder (local)  OR         │
                    │  Claude listwise rerank (+ "why")  │
                    └───────────────┬───────────────────┘
                    ┌───────────────▼───────────────────┐
                    │ [6] DIVERSIFY + PRESENT            │
                    │  MMR • thread-group • near-dup     │
                    │  collapse • snippet + highlight    │
                    └───────────────┬───────────────────┘
                                    ▼
                          Results (streamed best-first)
                                    │
        implicit feedback (open/reply/dwell/scroll/position) ──▶ trains L1/L2
```

---

## Stage 0 — Query Understanding

Turn raw input into a structured `QueryPlan` before any retrieval. Cheap, deterministic first; Claude only for genuinely ambiguous natural language (result cached).

**Steps:**

1. **Operator parse.** Extract structured operators from the query grammar (`from:`, `to:`, `subject:`, `has:attachment`, `before:`, `after:`, `is:unread`, `tag:`, `note:`, `in:INBOX`, `larger:`, `filename:`, quotes for phrases, `-` for negation). These become **hard filters** applied as `WHERE` constraints, not soft ranking signals.
2. **Intent classification.** Classify the free-text remainder:
   - `navigational / known-item` — "the invoice Acme sent last week" → favor exact + recency + sender affinity, tight result set.
   - `exploratory / topical` — "everything about the office move" → favor semantic recall + diversity (MMR), broader set.
   - `lookup / entity` — "tracking number for my order", "AWS bill" → favor entity index + structured.
   Intent shifts the fusion weights (below). A cheap local classifier (logistic over query features: length, has-operators, has-quotes, question-words, contains-known-contact) decides; Claude is a fallback.
3. **Spelling correction.** SymSpell/trigram against the **corpus vocabulary** (terms that actually appear in this mailbox), smart-case aware. "invoce" → "invoice". Original and corrected terms both retrieved; corrected boosted.
4. **Alias / contact resolution.** "Bob", "the Acme folks" → resolve against the contact graph to concrete addresses/domains, added as soft `from:`/`to:` boosts (not hard filters unless the user typed the operator).
5. **Query expansion.** Broaden recall without hurting precision:
   - **Synonyms** from local term co-occurrence (PMI) — mailbox-specific ("invoice" ~ "receipt" ~ "statement" if they co-occur here).
   - **Embedding neighbors** of the query string (the dense retriever handles most of this implicitly).
   - **Acronym/alias** expansion ("PR" → "pull request" only if evidenced in corpus).
   - **Claude expansion** (optional, cached): 3–5 alternative phrasings for hard exploratory queries.
6. **Embed the query** once (local model by default) → the vector used by the dense retriever and by cross-encoder features.
7. **NL → plan (Claude, cached).** If the input is clearly prose ("who did I forget to reply to about the lease?"), Claude compiles it to a confirmable `{filters, semantic_query, sort, intent}` plan. Deterministic operator syntax bypasses Claude entirely. The plan is cached by normalized query hash.

**Output — `QueryPlan`:**

```
QueryPlan {
  raw: String,
  hard_filters: Vec<Filter>,        // from:, has:, before:, tag:, in:  (WHERE)
  lexical_terms: Vec<Term>,         // corrected + original, per-field
  phrases: Vec<Phrase>,             // quoted, proximity-scored
  expansions: Vec<Term>,            // soft, down-weighted
  query_vector: Option<Vec<f32>>,   // dense retrieval
  entities: Vec<EntityRef>,         // resolved people/orgs/ids
  intent: Intent,                   // Navigational | Exploratory | Lookup
  sort: SortSpec,                   // relevance (default) | date | sender
  scope: Scope,                     // account(s), mailbox(es)
}
```

---

## Stage 1 — Candidate Generation (parallel recall)

Every retriever runs concurrently against the local index and returns its own top-N (default N=200) with a **source-local score** and rank. None is authoritative; fusion decides. Each is individually skippable (config/degradation).

| Retriever | Backend | Catches | Source score |
|---|---|---|---|
| **Lexical BM25** | FTS5, field-weighted (`bm25(subject,from,body,attach,notes,summary)`) | exact terms, phrases, proximity, operators | BM25 |
| **Dense vector kNN** | sqlite-vec `vec0`, cosine over message + chunk embeddings | paraphrase / semantic / no keyword overlap | cosine |
| **Fuzzy** | nucleo (subsequence) + trigram index | typos, partial words, subject/sender/contact | fuzzy score |
| **Entity match** | `entities`/`entity_mentions` | people, orgs, amounts, tracking #s, order/invoice IDs, IBANs | exact/normalized match |
| **Structured filter** | SQL over `messages` | `from:/to:/has:/before:/tag:/is:` — **hard constraints** | pass/fail (gates all others) |
| **Prefix / autocomplete** | FTS5 prefix + `finder_index` | as-you-type, incremental | prefix score |
| **Recency prior** | `messages(date)` index | recent mail with weak textual match (known-item bias) | recency decay |

Notes:

- **Hard filters gate everything.** Structured constraints are applied as a candidate mask; the other retrievers score only within the surviving set (or the mask is applied post-hoc when cheaper).
- **Field-weighted BM25** default column weights: `subject 8.0, from 4.0, to/cc 2.0, body 1.0, attachments 1.0, notes 3.0, ai_summary 2.0` (all TOML-tunable). A subject hit should beat a body hit.
- **Chunk-level dense retrieval:** long messages/attachments are chunked (512 tok, 64 overlap); kNN returns chunks, deduped to their parent message keeping `max` and `mean` chunk similarity as separate features.
- **Phrase & proximity:** quoted phrases use FTS5 `NEAR`/phrase queries; an unquoted multi-term query still earns a proximity bonus when terms appear close together.

---

## Stage 2 — Fusion & Dedup

Combine the ranked lists into one candidate set. Default: **weighted Reciprocal Rank Fusion (RRF)** — robust to incomparable source scores (BM25 magnitude vs cosine vs fuzzy), no per-source score normalization needed.

```
fused_score(m) =  Σ_over_sources s   w_s · 1 / (k_rrf + rank_s(m))
```

- `k_rrf` default 60. `rank_s(m)` is m's 1-based rank in source s (absent → term omitted).
- `w_s` are **intent-dependent** source weights:

| Source | Navigational | Exploratory | Lookup |
|---|---|---|---|
| Lexical BM25 | 1.0 | 0.7 | 0.8 |
| Dense vector | 0.6 | 1.0 | 0.5 |
| Fuzzy | 0.9 | 0.4 | 0.6 |
| Entity | 0.7 | 0.5 | 1.0 |
| Recency prior | 0.8 | 0.3 | 0.4 |

- **Dedup / collapse:** chunk hits → parent message; optionally messages → thread (thread-mode shows the best representative message, with a "+N in thread" affordance). Near-duplicate bodies (bulk newsletters, quoted replies) collapse via SimHash so one query doesn't return ten copies.
- **Also-consider alternative:** a normalized weighted linear blend (`Σ w_s · minmax(score_s)`) is available (`fusion = "linear"`) for cases where absolute scores matter; RRF is the default.
- Output: ~200–500 fused candidates carrying every source's rank+score (needed as ranking features).

---

## Stage 3 — Feature Extraction

Build a feature vector per candidate. Features fall into textual-match, semantic, behavioral/personal, temporal, status, structural, and global-prior groups. All are cheap to compute from the local DB + the fused metadata.

| Feature | Group | Meaning |
|---|---|---|
| `bm25_subject`,`bm25_body`,`bm25_from`,`bm25_attach` | textual | per-field BM25 |
| `exact_phrase_hit` | textual | query phrase appears verbatim |
| `term_coverage` | textual | fraction of query terms present |
| `proximity_min_span` | textual | tightest window covering all terms |
| `best_match_field` | textual | subject / from / body / attachment (categorical) |
| `cos_max_chunk`,`cos_mean_chunk` | semantic | max/mean dense similarity |
| `fuzzy_score` | textual | best fuzzy subsequence score |
| `rrf_score`,`num_sources_hit`,`best_source` | fusion | how the retrievers agreed |
| `sender_affinity` | personal | msgs exchanged × reply-ratio × recency of last interaction |
| `user_replied_thread` | personal | user has replied in this thread |
| `prior_opens_from_sender` | personal | historical open rate from this sender |
| `thread_activity` | personal | recent traffic in the thread |
| `age_days`,`recency_decay` | temporal | `exp(-age/half_life)` |
| `matches_date_intent` | temporal | message date matches parsed date scope |
| `is_unread`,`is_flagged`,`is_pinned` | status | flags |
| `ai_priority` | status | triage importance (0..1) |
| `has_tag_match` | status | a query term matches an applied tag |
| `folder_prior` | status | Inbox > Archive > Spam prior |
| `has_attachment_match` | structural | matched term is in an attachment |
| `is_thread_root`,`thread_size`,`msg_length` | structural | shape |
| `sender_reputation`,`is_newsletter`,`is_automated` | global | down-weight bulk/automated unless asked |

Feature values are logged alongside impressions so the exact vector that produced a ranking can be replayed for training and debugging.

---

## Stage 4 — L1 Ranker (fast, learned)

A fast model scores **all** fused candidates from their feature vectors and keeps the top-K (default 50).

- **Model:** gradient-boosted decision trees (LambdaMART-style, pairwise/listwise objective) when a trained model exists; otherwise a hand-tuned **linear scorer** (cold-start, below). Inference is microseconds/candidate — pure Rust, no FFI on the hot path.
- **Training target:** learning-to-rank from implicit feedback (Stage: feedback loop). Optimizes NDCG.
- **Cold-start deterministic scorer** (used until enough feedback is collected, and as the always-available fallback):

```
score = 1.00 * rrf_score
      + 0.90 * bm25_subject      + 0.35 * bm25_body
      + 0.80 * cos_max_chunk     + 0.30 * cos_mean_chunk
      + 0.60 * exact_phrase_hit  + 0.40 * term_coverage
      + 0.50 * sender_affinity   + 0.30 * user_replied_thread
      + 0.45 * recency_decay     + 0.25 * ai_priority
      + 0.20 * is_flagged        + 0.15 * is_unread
      + 0.15 * has_tag_match     + 0.20 * has_attachment_match
      - 0.40 * is_newsletter     - 0.25 * is_automated       (unless query is topical/bulk)
```

All weights are TOML-overridable and are the initial values a learned model refines.

---

## Stage 5 — L2 Reranker (expensive, top-K only)

Re-order only the top-K (≈50) with a heavier model that reads actual text, not just features. Two interchangeable backends:

1. **Local cross-encoder** (default when enabled): a small local reranker (e.g. a MiniLM/bge-reranker via ONNX) scores `(query, message_text)` pairs jointly — far more precise than bi-encoder cosine, cheap enough for 50 pairs (<80 ms), fully offline, zero egress.
2. **Claude listwise rerank** (opt-in, highest quality): send the top ~30 candidates (subject + snippet + key metadata, redacted) to `claude-haiku-4-5`/`claude-sonnet-5` and ask for a listwise ordering **plus a one-line "why this matched"** per result. Structured output, cached by `(query_hash, candidate_id_set)`. Degrades to the L1 order on error/budget.

Reranking is gated by `search.rerank` (`off | cross_encoder | claude | auto`). `auto` uses the cross-encoder for interactive typing and Claude for explicit "deep search" / `mail ask`.

---

## Stage 6 — Diversify & Present

- **MMR (Maximal Marginal Relevance)** for exploratory intent: greedily pick results maximizing `λ·relevance − (1−λ)·max_similarity_to_already_picked` so the top-10 isn't ten near-identical newsletters. `λ` default 0.7; disabled for navigational intent (where the user wants the single best match first).
- **Thread grouping:** in thread-mode, collapse a thread to its best-scoring message with an inline count; expandable.
- **Near-duplicate collapse:** SimHash clusters; show the canonical (usually newest) with a "N similar" chip.
- **Snippet + highlight:** best-matching span extracted (FTS5 `snippet()` for lexical, best chunk for semantic), query terms highlighted using match positions.
- **Streaming:** results flush best-first in score-ordered batches so the top result paints in <30 ms even while lower ranks are still being reranked.

---

## Personalization & Implicit-Feedback Learning Loop

Relevance improves with use. Every search interaction is logged and periodically distilled into ranker weights.

**Signals logged per query (impressions + actions):**

- Which results were **shown** (impressions) and at what **position**.
- Which were **opened**, **replied to**, **archived-from-results**, **dwelled on** (and how long), **scrolled past**.
- The **feature vector** of every impression (for exact replay).

**Turning clicks into labels:**

- A result opened at position *p* over results skipped above it is a positive pairwise preference (`clicked ≻ skipped-above`).
- **Position-bias correction:** weight labels by inverse propensity (a result clicked at position 8 is a stronger signal than one clicked at position 1). A simple position-based click model estimates examination propensity.
- Reply/long-dwell > open > hover; archive-from-results is a mild negative for that query.

**Training:**

- A nightly (or on-demand) local job trains the L1 GBDT / updates linear weights on the accumulated pairs, evaluates on a held-out slice, and **hot-swaps** the model only if offline NDCG improves (guardrail against regressions). Old model kept for rollback.
- Fully local — no data leaves the machine. Personalization is per-user and per-mailbox-profile.
- Cold users fall back to the deterministic scorer; personalization phases in as data accrues.

**Data model:**

```sql
CREATE TABLE search_log (
  query_id     INTEGER PRIMARY KEY,
  raw_query    TEXT NOT NULL,
  norm_hash    BLOB NOT NULL,
  intent       TEXT,
  issued_at    INTEGER NOT NULL,
  result_count INTEGER
);
CREATE TABLE search_impression (
  query_id     INTEGER NOT NULL REFERENCES search_log(query_id) ON DELETE CASCADE,
  message_uid  INTEGER NOT NULL,
  position     INTEGER NOT NULL,
  features     BLOB NOT NULL,          -- serialized feature vector
  l1_score     REAL,  l2_score REAL,
  PRIMARY KEY (query_id, message_uid)
);
CREATE TABLE search_action (
  query_id     INTEGER NOT NULL REFERENCES search_log(query_id) ON DELETE CASCADE,
  message_uid  INTEGER NOT NULL,
  action       TEXT NOT NULL,          -- open|reply|archive|dwell|scroll_past
  dwell_ms     INTEGER,
  at           INTEGER NOT NULL
);
CREATE TABLE ranker_model (
  id           INTEGER PRIMARY KEY,
  kind         TEXT NOT NULL,          -- linear|gbdt
  blob         BLOB NOT NULL,          -- serialized model / weights
  trained_at   INTEGER NOT NULL,
  offline_ndcg REAL,
  active       INTEGER NOT NULL DEFAULT 0
);
```

Logging is strictly opt-outable (`search.learning = false`); it is local telemetry, never transmitted.

---

## Explainability — "Why this matched"

Every result can be expanded to show its ranking rationale:

- Top contributing features (e.g. *"subject match + you reply to this sender often + 2 days old"*).
- Which retrievers surfaced it (lexical + semantic agreement is a strong trust signal).
- The matched span(s).
- If Claude reranked: its one-line reason.

Exposed as `--explain` (CLI), an inline expander (TUI `x`), and an `explain` block on the gRPC/MCP result. This makes the ranker debuggable and gives agents grounding for citations.

---

## Query Language / Operators (grammar)

Hard filters compose with free text; free text is ranked, filters constrain.

```
from:alice            to:me            cc:team@x.com
subject:invoice       body:"exact phrase"
has:attachment        filename:*.pdf   larger:5mb   smaller:1mb
before:2025-01-01     after:last-week  on:2026-07-01   date:2025-06..2025-08
is:unread  is:read  is:flagged  is:pinned  is:replied  is:muted
tag:work   tag:project/*   -tag:newsletter
note:contract          has:note   has:tag
in:INBOX   in:Archive   account:Personal   thread:<id>
ai:needs-reply   ai:priority>high   ai:category:invoice   ai:sentiment:negative
~semantic phrase       (force semantic)      =exact terms   (force lexical)
"multi word phrase"    -excludeterm          project alpha  (free text, ranked)
```

- Unknown `key:value` with no registered operator is treated as free text (never an error).
- `mail search` uses the full pipeline; `mail search --nl "…"` forces Claude query compilation with a confirmable plan.

---

## Saved Searches & Smart Folders

- **Saved search:** a named query string; re-run through the full pipeline on demand.
- **Smart folder (virtual mailbox):** a saved query re-evaluated on every sync so membership stays live; may be defined in natural language and compiled once by Claude into a stored hybrid plan (`from:` + FTS + embedding predicate). No mail is moved on the server. Smart folders can trigger actions (auto-tag/notify) on new matches.

---

## Evaluation Harness & Metrics

Relevance is measured, not asserted.

- **Golden set:** a versioned file of `(query, judged-relevant message-ids)` maintained per developer mailbox; `mail search eval` reports **NDCG@10, MRR, Recall@50, P@3**.
- **Online metrics** from the feedback log: click-through rate, mean reciprocal rank of the clicked result, "success@1/3" (did they open a top-1/top-3 result), abandonment rate.
- **Offline replay / shadow ranking:** a candidate ranker is scored against logged impressions+clicks before it's ever shown; A/B by deterministically bucketing queries. A new model ships only on a measured win.
- **Regression guard:** CI runs the golden set; a drop in NDCG fails the build.

---

## Caching & Incrementality

- **Query-plan cache** — NL→plan and expansion results keyed by normalized query hash (Claude calls are the expensive part).
- **Embedding cache** — query and document embeddings persisted; documents re-embedded only on `content_hash` change.
- **Result cache** — `(query, filter, corpus_version)` → ranked ids, invalidated when the corpus version bumps (new mail) or the active ranker changes. Newly-synced mail can **bypass** the cache so fresh mail is never hidden by a stale cached result.
- **Warm the query embedder** at daemon start.

---

## Config (TOML)

```toml
[search]
default_mode   = "hybrid"        # lexical | semantic | hybrid
fusion         = "rrf"           # rrf | linear
rrf_k          = 60
rerank         = "auto"          # off | cross_encoder | claude | auto
candidates_per_source = 200
top_k_rerank   = 50
learning       = true            # implicit-feedback personalization
mmr_lambda     = 0.7
default_limit  = 25

[search.bm25_weights]
subject = 8.0
from    = 4.0
to      = 2.0
body    = 1.0
attachments = 1.0
notes   = 3.0
ai_summary = 2.0

[search.fusion_weights.navigational]
lexical = 1.0
dense   = 0.6
fuzzy   = 0.9
entity  = 0.7
recency = 0.8
[search.fusion_weights.exploratory]
lexical = 0.7
dense   = 1.0
fuzzy   = 0.4
entity  = 0.5
recency = 0.3
[search.fusion_weights.lookup]
lexical = 0.8
dense   = 0.5
fuzzy   = 0.6
entity  = 1.0
recency = 0.4

[search.rank_weights]              # cold-start deterministic scorer (learned model overrides)
rrf_score = 1.0
bm25_subject = 0.9
cos_max_chunk = 0.8
exact_phrase_hit = 0.6
sender_affinity = 0.5
recency_decay = 0.45
ai_priority = 0.25
is_newsletter = -0.40

[search.reranker]
cross_encoder_model = "bge-reranker-base"    # ONNX, local
claude_model        = "claude-haiku-4-5"
claude_max_candidates = 30

[search.expansion]
synonyms   = true       # local co-occurrence
claude     = false      # Claude query expansion (opt-in, cached)
spellfix   = true
```

---

## CLI

```
mail search "invoice acme"                 # full pipeline, ranked
mail search "office move" --explore        # force exploratory weighting + MMR
mail search "from:alice invoice" --explain # show ranking rationale
mail search --nl "who did I forget to reply to about the lease?"
mail search "~contract termination clause" # force semantic
mail search "=Q3 report"                   # force exact/lexical
mail search "invoice" --rerank claude      # force Claude listwise rerank
mail search "invoice" --json               # scriptable, includes scores + why
mail search eval                           # NDCG/MRR/Recall on the golden set
mail similar 123                           # embedding kNN neighbors of a message
mail ask "how much did AWS bill me in Q2?" # RAG (retrieval + grounded answer)
```

`--json` result item:

```json
{
  "message_uid": 4471,
  "subject": "Invoice #338 — Acme",
  "from": "billing@acme.com",
  "date": "2026-06-30T10:12:00Z",
  "score": 18.42,
  "snippet": "Your invoice for June is attached. Total $4,200 …",
  "sources": ["lexical", "dense", "entity"],
  "why": "subject match • semantic match • you reply to this sender often • 25 days old"
}
```

---

## TUI

- `/` opens ranked incremental search; results stream in as you type (debounced ~25 ms; each keystroke cancels the prior in-flight ranking).
- Prefix toggles: `~` semantic, `=` exact; operators autocomplete (`from:` offers contacts, `tag:` offers tags).
- `x` on a result expands its **why-ranked** panel.
- `Ctrl-P` opens the fuzzy finder (Part III) — the instant "jump anywhere" complement to full search.
- Result rows show source glyphs (lexical/semantic/entity agreement), highlighted matched span, and status (unread/flagged/tag chips).
- Opening/replying to a result feeds the learning loop transparently.

---

## gRPC Surface

```proto
service SearchService {
  // Streams ranked hits best-first as the pipeline produces them.
  rpc Search        (SearchRequest) returns (stream SearchHit);
  rpc Semantic      (SemanticRequest) returns (stream SearchHit);   // dense only
  rpc CompileQuery  (CompileQueryRequest) returns (QueryPlan);      // NL→plan (Claude)
  rpc Explain       (ExplainRequest) returns (RankExplanation);     // why a result ranked
  rpc SaveSearch    (SaveSearchRequest) returns (SavedSearch);
  rpc LogFeedback   (FeedbackRequest) returns (Empty);              // impressions/actions
  rpc Evaluate      (EvaluateRequest) returns (EvalReport);         // NDCG/MRR/Recall
}

message SearchRequest {
  string query      = 1;
  string filter     = 2;                     // operator DSL
  Mode   mode       = 3;                      // CONFIG|LEXICAL|SEMANTIC|HYBRID
  Rerank rerank     = 4;                      // OFF|CROSS_ENCODER|CLAUDE|AUTO
  Intent intent     = 5;                      // AUTO|NAVIGATIONAL|EXPLORATORY|LOOKUP
  uint32 limit      = 6;
  bool   explain    = 7;                      // include per-hit rationale
  int64  account_id = 8;                      // 0 = all
}

message SearchHit {
  Message message      = 1;
  double  score        = 2;                   // final rank score
  string  snippet      = 3;                   // highlighted
  repeated string sources = 4;                // which retrievers surfaced it
  RankExplanation why  = 5;                   // present when explain=true
}

message RankExplanation {
  repeated FeatureContribution features = 1;  // name, value, weighted_contribution
  string claude_reason = 2;                   // if Claude reranked
  repeated uint32 highlight_spans = 3;
}
```

Streaming = first hit reaches the client in <30 ms; a fresh keystroke cancels the prior stream. Search is the same core call the CLI, TUI, and MCP all use.

---

## MCP Tools (search)

- **`search_mail`** `{query, filter?, mode?, limit?, explain?}` → ranked hits with `score`, `snippet`, `sources`, `why`. The primary tool Claude uses to find mail — it gets the *ranked* set, not a raw dump, so the most relevant message is first.
- **`semantic_search`** `{query, k, filter?}` → dense kNN hits (paraphrase recall).
- **`ask_mailbox`** `{question, top_k?, filter?}` → retrieval-augmented, grounded answer with `citations:[{message_uid, quote}]`. Built on the same pipeline (retrieve → rerank → generate).
- **`explain_ranking`** `{query, message_uid}` → why a specific message ranked where it did.
- **`similar_messages`** `{message_uid, k}` → nearest neighbors.

Because MCP tools are auto-projected from the gRPC services, the agent's search is *exactly* the human's search — same recall, same fusion, same ranker.

---

## Performance Budget (per query, 100k-message mailbox, M-series Mac)

| Stage | Target |
|---|---|
| Query understanding (deterministic) | < 3 ms |
| Query understanding (Claude NL compile, cached miss) | < 400 ms (only for prose; cached after) |
| Candidate generation (all retrievers, parallel) | < 25 ms |
| Fusion + dedup | < 5 ms |
| Feature extraction | < 8 ms |
| L1 ranker (all candidates) | < 5 ms |
| L2 cross-encoder rerank (top-50) | < 80 ms |
| L2 Claude listwise rerank (top-30) | provider latency (streamed) |
| **First streamed hit visible** | **< 30 ms** |
| **Full ranked result (no Claude rerank)** | **< 150 ms** |

Tactics: parallel retrievers on a `rayon`/`tokio` pool; WAL-mode read pool so search never blocks on indexing writes; bounded top-K heaps (no full sorts); pre-folded match blobs; query-generation token to cancel superseded scans; cross-encoder on a dedicated blocking thread pool; Claude rerank only on explicit deep-search.

---

## Edge Cases

- **Empty query** → recency-ranked recent mail (a useful default inbox view), bounded.
- **No lexical matches** → semantic + fuzzy still return candidates; UI offers "search semantically" if all sources are weak.
- **Embeddings unavailable / no key** → dense retriever silently drops; lexical+fuzzy+entity fuse and rank well; a badge notes reduced recall.
- **No trained ranker / cold user** → deterministic scorer; still strong.
- **Index cold (initial sync)** → fall back to live SQL `LIKE`/FTS over what's synced; swap to full pipeline once indexed; recent/inbox mail is indexed first so results are useful early.
- **Huge result sets** → streamed + paginated; never materialize all.
- **Ambiguous NL** → Claude plan surfaced for confirmation rather than silently guessing.
- **Offline** → fully functional; only Claude rerank/NL-compile degrade to local paths.
- **Adversarial content** (SEO-stuffed newsletters) → `is_newsletter`/`sender_reputation` down-weight; MMR prevents flooding.

---
---

# PART II — FEATURE CATALOG (66 New Features)

Priorities: **P0** = core to the bridge / ship early, **P1** = high value, **P2** = later. Each lists its programmatic surface (gRPC / MCP / CLI).

## AI & Claude Bridge

1. **Thread Summarizer** (P0) — Collapses a full thread into a cached, streaming structured digest (TL;DR, decisions, open questions, action items with owners), recomputed only when a new reply lands, escalating Haiku→Sonnet by token budget. — *grpc: AiService.SummarizeThread (stream); mcp: summarize_thread; cli: mail summarize <thread-id> --stream*
2. **Inbox Triage Engine** (P0) — Runs each newly synced message through Claude Haiku to assign category, urgency score, needs-reply flag, and a one-line reason, stored as queryable labels powering a sortable Triage view and `triage:` filters. — *grpc: TriageService.TriageMessages/StreamTriage; mcp: triage_inbox, get_triage; cli: mail triage [--since 1d]*
3. **Mailbox RAG Ask** (P0) — Answers natural-language questions over the whole local mailbox by fusing FTS5 recall with embeddings, feeding top-k chunks to Claude, and streaming a cited answer linked back to message-ids under optional scope. — *grpc: AiService.AskInbox (stream); mcp: ask_mailbox; cli: mail ask "<question>"*
4. **Structured Data & Entity Extractor** (P1) — Extracts typed structured data and normalized entities (invoice amount/due/vendor, flight PNR, meeting datetime, people, amounts, tracking numbers, action items) against a JSON schema, validated and stored in a queryable table. — *grpc: MailService.ExtractStructured / SearchService.SearchEntities; mcp: extract_data, extract_entities, search_entities; cli: mail extract <id> --schema invoice*
5. **Prompt Library** (P1) — Named, parameterized prompt templates with a declared output schema, each auto-registered as its own MCP tool so users and agents invoke reusable AI operations by name. — *grpc: PromptService.ListPrompts/RunPrompt; mcp: run_prompt + per-template tools; cli: mail prompt run <name> --on <target>*
6. **Conversation Memory Store** (P2) — Editable per-thread and per-sender AI memory (prior Q&A, corrections, tone preferences, standing facts) injected into future summaries, drafts, and agent runs, with background compaction. — *grpc: MemoryService.Get/Put/List; mcp: get_memory, add_memory; cli: mail memory show/add <sender>*

## Search, Fuzzy Find & Indexing

7. **Natural-Language Query Compiler** (P0) — Claude translates plain English into a confirmable, cached FTS5 match string plus date/flag/amount filters and folder scope, falling back to raw FTS when unavailable. — *grpc: SearchService.CompileQuery; mcp: compile_query; cli: mail search --nl '<phrase>'*
8. **Semantic Vector Search** (P0) — Embeds every message chunk (local model by default) into a sqlite-vec table for cosine-kNN so "that email about the office move" finds "relocating HQ" with no keyword overlap. — *grpc: SearchService.SemanticSearch; mcp: semantic_search; cli: mail search --semantic '<text>'*
9. **Incremental Indexing Pipeline** (P0) — A tokio background pipeline maintains FTS5, vector, and entity indexes idempotently keyed by UID + content hash, using a durable job queue with checkpoints and per-stage rebuild. — *grpc: IndexService.Status/Reindex; mcp: index_status; cli: mail index rebuild [--stage fts|vec|entity]*
10. **Hybrid Ranker with Claude Rerank** (P0) — Fuses BM25 and vector via RRF, then optionally Claude-listwise-reranks the top ~30 with per-result relevance and a "why this matched", degrading gracefully to pure RRF. — *grpc: SearchService.Search(mode=HYBRID, rerank=true); mcp: hybrid_search; cli: mail search --hybrid --rerank*
11. **Fuzzy Finder (fzf-style)** (P0) — Interactive incremental fuzzy matcher over unified sources — messages, folders, contacts, saved searches, tags, commands — with live preview, multi-select, and a streaming CLI mode. — *grpc: FinderService.Fuzzy (stream); cli: mail find [--source …]; TUI: Ctrl-P*

## Tags, Notes & Organization

12. **AI Auto-Tagging** (P0) — On each sync, Claude classifies new mail against the user's colored, NL-defined tag taxonomy into pending suggestions, auto-applying high-confidence tags and learning from accept/reject decisions. — *grpc: TagService.SuggestTags/ApplyTags/ListTags; mcp: suggest_tags, apply_tags; cli: mail tag suggest/apply <id>*
13. **Natural-Language Smart Folders** (P0) — A virtual mailbox defined by a plain-English predicate that Claude compiles once into a stored hybrid query, re-run cheaply on every sync so membership stays live without moving mail on the server. — *grpc: SmartFolderService.Create/List/Members; mcp: create_smart_folder, list_smart_folders; cli: mail folder new "<nl-query>"*
14. **Message & Thread Annotations** (P1) — Freeform notes on any message or thread, FTS5-searchable and shown inline, with an option for Claude to draft a grounded annotation that becomes an editable note. — *grpc: AnnotationService.AddNote/ListNotes/DraftNote; mcp: add_note, draft_note; cli: mail note add/ai <id>*
15. **AI-Assisted Kill-File / Mute** (P1) — Muting a sender/thread/pattern auto-archives matching mail reversibly; Claude mines archive/delete behavior to propose concrete one-key mute rules. — *grpc: MuteService.Mute/SuggestRules; mcp: mute, suggest_mute_rules; cli: mail mute <target> / mail mute suggest*
16. **Natural-Language Bulk Operations** (P1) — One command selects a message set by plain-English query and applies an action transactionally with a printed preview count and an undo token. — *grpc: BulkService.Preview/Apply/Undo; mcp: bulk_preview, bulk_apply; cli: mail bulk "<nl-selection>" --do <tag|archive|mute>*

## Compose, Schedule & Send

17. **Scheduled Send & Durable Outbox** (P0) — Outgoing mail written to a durable SQLite outbox with `send_at`, dispatched by a background SMTP worker that survives restarts and offline periods, with retry, edit/reschedule/cancel, and a recall window. — *grpc: OutboxService.ScheduleSend/List/Cancel/Reschedule/UndoSend; mcp: schedule_send, list_outbox, undo_send; cli: mail send --at <ts>, mail outbox, mail undo*
18. **AI Reply Drafting** (P0) — Claude reads the full local thread plus samples of the user's own past replies to that correspondent and a short intent to generate an on-voice reply with correct headers, staged as an editable draft that never auto-sends. — *grpc: DraftService.DraftReply (stream); mcp: draft_reply; cli: mail reply <id> --ai "…"*
19. **Tone & Length Rewrite** (P1) — Claude rewrites a draft or selection to a target register (formal, warmer, firmer, shorter) or mirrors the recipient's style, returned as a cyclable, revertible revision. — *grpc: DraftService.RewriteDraft; mcp: rewrite_draft; cli: mail draft rewrite <id> --tone formal --shorter*
20. **Pre-Send Guardian** (P1) — At send time Claude flags "see attached" with no attachment, wrong/extra recipients, unfilled placeholders, apparent secrets, and tone clashes, blocking or warning by severity. — *grpc: OutboxService.PreflightCheck; mcp: preflight_check; cli: mail draft check <id> (auto on send)*
21. **AI Follow-up & Waiting-On Tracker** (P1) — Claude judges whether a sent message expects a reply, extracts the ask, records a deadline, and surfaces an aging "waiting-on" list, drafting a ready nudge. — *grpc: FollowupService.TrackThread/ListDue/ListWaitingOn; mcp: list_followups, list_waiting_on, draft_followup; cli: mail followups, mail waiting-on*
22. **Mail-Merge with Per-Recipient AI Personalization** (P1) — From a recipient list and template, Claude personalizes each message using local mail history and queues them to the outbox, with a dry-run preview of every rendered message. — *grpc: MergeService.PreviewMailMerge/RunMailMerge; mcp: mail_merge; cli: mail merge --template <t> --recipients <query|csv> --at <time> [--dry-run]*

## gRPC & Programmatic API

23. **Feature-parity MailService** (P0) — One internal command enum backs a single set of protos mirroring every CLI/TUI capability 1:1, with tonic, clap, and MCP as thin adapters and CI that fails if any core command has no RPC. — *grpc: MailService.* / AiService.* / SyncService.* / AdminService.*; cli: mail api serve*
24. **Token-streaming AI RPCs** (P0) — Server-streaming RPCs relay Claude Messages API tokens as typed frames (Token, ToolUseStart, Usage, Done), aborting the upstream request on client cancellation or deadline. — *grpc: AiService.*(stream); mcp: summarize_thread, draft_reply; cli: --stream*
25. **Unified Event Subscription Stream** (P0) — A single SubscribeEvents RPC delivers a typed union of domain events (NewMessage, FlagsChanged, SyncProgress, AiJobUpdated, DraftSaved) with a monotonic cursor backed by a SQLite event log so a reconnecting client resumes without gaps. — *grpc: MailService.SubscribeEvents; mcp: subscribe_events; cli: mail watch --query <q>*
26. **gRPC-to-MCP Tool Auto-Projection** (P0) — MCP tools are generated at runtime from the compiled gRPC descriptor set plus per-RPC annotations; each safe RPC becomes one MCP tool, mutating RPCs gated by scope — a new RPC adds an MCP tool with zero extra code. — *grpc: reflection; mcp: (all tools, generated); cli: mail mcp serve --stdio|--sse*
27. **Scoped Capability Tokens for Agents** (P0) — Bearer tokens minted with fine-grained scopes (mail.read, mail.send, ai.invoke, ai.spend:<cap>, mailbox:<name>) enforced by a tonic interceptor, so an autonomous Claude loop physically cannot email unless granted. — *grpc: AdminService.MintToken/RevokeToken/ListTokens; cli: mail token create --scope … --ttl 24h*
28. **Async AI Job Service with Resumable Progress** (P1) — Long-running AI work (summarize a 2,000-message inbox, backfill embeddings, batch-triage) as a durable job with streaming progress, reattachable after restart, cost-capped, using the Anthropic Message Batches path when latency-insensitive. — *grpc: AiService.SubmitJob/WatchJob/CancelJob/ListJobs; mcp: submit_ai_job, watch_ai_job; cli: mail ai job submit …*

## Sync, Protocols & Accounts

29. **IMAP IDLE Push Sync Engine** (P0) — Long-lived IDLE connections per high-priority folder so new mail, flag changes, and expunges push within seconds, degrading transparently to polling when IDLE is unavailable. — *grpc: SyncService.WatchEvents (stream); cli: mail sync --watch*
30. **CONDSTORE/QRESYNC Delta Sync** (P0) — Persists per-folder UIDVALIDITY and HIGHESTMODSEQ and issues QRESYNC / FETCH CHANGEDSINCE so only changes transfer, falling back to a UID-window diff on servers lacking both. — *grpc: SyncService.SyncFolder; cli: mail sync [--full]*
31. **OAuth2 Broker for Gmail & Outlook** (P0) — Loopback-redirect OAuth2 + PKCE for Google and Microsoft, refresh tokens in the Keychain, XOAUTH2 SASL for IMAP/SMTP, refresh-before-expiry, re-consent on revocation. — *grpc: AccountService.BeginOAuth/CompleteOAuth/RefreshToken; cli: mail account login --oauth <provider>*
32. **AI Account Autoconfig** (P1) — Given only an email address, probes ISPDB / SRV / autodiscover and, on a miss, hands the domain + MX + probe responses to Claude to infer IMAP/SMTP settings, validates by login, and writes a ready TOML block. — *grpc: AccountService.Autoconfigure; mcp: add_account; cli: mail account add <email>*
33. **Unified Inbox Virtual Mailbox** (P0) — A synthetic unified mailbox merging every account's Inbox into one time-ordered, Message-ID-deduplicated view, with actions routed back to the correct account/folder. — *grpc: MailService.ListUnified; mcp: list_unified_inbox; cli: mail list --all*

## TUI/CLI Experience

34. **Natural-Language Command Palette** (P0) — A Ctrl-P fuzzy palette over every action/keybinding/saved-search/mailbox; input matching no command is streamed to Claude which returns a typed CommandInvocation the palette previews and executes. — *grpc: CommandService.ResolveIntent; mcp: resolve_intent; cli: mail do "<natural language>"*
35. **Contextual Ask Pane** (P0) — A toggleable side pane with a streaming Claude chat that auto-injects the selected message/thread as context and renders answers token-by-token with citations. — *grpc: AssistantService.Chat (stream); mcp: ask_about_message; cli: mail ask <id> "<question>"*
36. **Modal Vim Keybindings Engine** (P0) — A layered keymap engine (normal/insert/visual, chord sequences), fully rebindable and hot-reloadable via keys.toml, mapping to named action ids shared by palette, gRPC, and MCP. — *grpc: ConfigService.GetKeymap/SetBinding; cli: mail keys set <chord> <action>*
37. **Scriptable Structured Output** (P0) — A global `--format {table,json,ndjson}` on every command emits stable serde schemas; streaming commands emit ndjson lines mirroring the gRPC frames, with stable exit codes for pipelines/CI. — *cli: global --format*
38. **AI Quick-Action Menu** (P1) — Pressing `.` on a message opens a menu topped by Claude-suggested actions specific to it ("draft a decline", "extract the invoice total", "unsubscribe"), each one keypress from execution. — *grpc: AssistantService.SuggestActions; mcp: suggest_actions; cli: mail actions <id>*

## Security, Privacy & AI Safety

39. **PII Redaction Firewall** (P0) — A mandatory pre-flight pass on every body/thread before any Claude call detects and reversibly tokenizes emails, phones, cards (Luhn), addresses, secrets, names, then re-hydrates Claude's response so the user sees real values but the API never does. — *grpc: AiPolicyService.RedactPreview/SetRedactionLevel; mcp: redact_preview; cli: mail ai policy set-redaction <account> strict*
40. **AI Call Audit Ledger** (P0) — An append-only table recording every Claude request (timestamp, ids, model, tokens, cost, redaction level, latency, SHA-256 of the exact payload sent), with each AI artifact linking to its ledger entry. — *grpc: AuditService.QueryAiCalls/ExportLedger; mcp: query_ai_audit; cli: mail ai audit [export]*
41. **AI Policy & Data-Residency Engine** (P0) — Declarative TOML marks accounts/folders/patterns as allowed/local-only/forbidden with a residency tag; every AI path consults it first; forbidden folders are invisible to AI features; resolution is logged and explainable. — *grpc: AiPolicyService.Evaluate/SetAiMode; mcp: evaluate_ai_policy; cli: mail ai policy set <target> --mode local-only, mail ai policy explain <id>*
42. **Token & Dollar Budget Enforcer** (P1) — Per-account and global daily/monthly token and dollar caps checked before dispatch; soft caps auto-downgrade the model (opus→sonnet→haiku), hard caps block; bulk jobs get a separate sub-budget. — *grpc: AiPolicyService.SetBudget/GetSpend; mcp: get_ai_budget; cli: mail ai budget set/status*
43. **Prompt-Injection Shield** (P1) — Every body is wrapped in untrusted-content delimiters and scanned for injection patterns (hidden text, zero-width chars, "ignore previous instructions"); detected messages are flagged and any AI action on them requires confirmation, logged. — *grpc: AiSafetyService.ScanInjection; mcp: scan_prompt_injection; cli: mail ai scan-injection <id>*
44. **Local-Only Model Path** (P1) — A fully on-device inference route (candle/llama.cpp for generation, local embeddings for search) exposing the same summarize/embed/draft verbs, forced by policy for local-only mail, labeling outputs as locally generated with zero egress. — *grpc: AiService.Summarize{provider=local}/Embed; cli: mail ai provider set <account> local*

## Automation, Rules & Hooks

45. **AI Classification Rules Engine** (P0) — TOML rules combine deterministic predicates with a `claude_is` natural-language predicate ("a cold sales pitch") and an actions block, caching classification per message-id + prompt-hash. — *grpc: RuleService.CreateRule/ListRules/EvaluateRules; mcp: create_rule, run_rules_on_query; cli: mail rule add / run*
46. **Natural-Language Rule Synthesis** (P1) — Given a plain-English instruction, Claude generates a concrete rule preferring cheap deterministic predicates, returning it with a dry-run over the last N days showing exactly what it would have hit. — *grpc: RuleService.SynthesizeRule; mcp: synthesize_rule; cli: mail rule new "<description>"*
47. **Autonomous Inbox Agent** (P1) — A scheduled/event-driven agentic loop where Claude calls a constrained toolset (archive, label, snooze, draft-reply, escalate) toward a user policy, dry-run by default, every action logged with its reason, requiring an allowlist to mutate. — *grpc: AgentService.RunInboxAgent/GetAgentRunLog; mcp: run_inbox_agent; cli: mail agent run [--dry-run] / log*
48. **Event Hook Dispatcher** (P0) — Config-driven shell commands fire on mail events (on_new_message, on_label, on_move, on_rule_match, on_sync_error), passing the event as JSON on stdin, run in a bounded pool with timeouts. — *grpc: HookService.ListHooks/TestHook; mcp: list_hooks, test_hook; cli: mail hook add <event> -- <cmd>*
49. **Outbound Webhooks with AI-Enriched Payloads** (P1) — Registered endpoints receive HMAC-signed JSON with retries and a persisted delivery queue; payloads can include a Claude summary and extracted fields so Slack/ticketing/n8n get AI-enriched data. — *grpc: WebhookService.Register/List/ReplayDelivery; mcp: register_webhook; cli: mail webhook add <url> --events …*
50. **Rule Backtest & Explain** (P1) — Runs a rule/ruleset in dry-run over a historical query reporting per-message what would have happened, a one-line Claude explanation per `claude_is` decision, and aggregate hit/cost stats; corrections become few-shot examples. — *grpc: RuleService.BacktestRule; mcp: backtest_rule; cli: mail rule backtest <name> --since 30d --explain*

## Attachments & Extraction

51. **Attachment Text Extraction Pipeline** (P0) — On sync every attachment (PDF, DOCX, XLSX, PPTX, TXT, HTML, CSV) is routed to a format-specific extractor emitting plain text plus per-page offsets into a table mirrored to FTS5, content-hash keyed. — *grpc: AttachmentService.ExtractText/GetExtractedText; mcp: get_attachment_text; cli: mail attach text <msg>:<part>*
52. **OCR for Images & Scanned PDFs** (P0) — Image attachments and text-less PDFs pass through OCR (Apple Vision default, Tesseract fallback) to produce searchable text with bounding boxes, recording native-vs-OCR provenance and confidence. — *grpc: AttachmentService.Ocr; mcp: ocr_attachment; cli: mail attach ocr <msg>:<part>*
53. **Structured Receipt/Invoice Extraction** (P0) — Detects invoice/receipt attachments and sends text + page image to Claude with a strict schema to pull vendor, number, line items, totals, currency, due date, and status into a queryable, CSV-exportable table. — *grpc: AttachmentService.ExtractInvoice/ExportInvoices; mcp: extract_invoice, list_invoices; cli: mail invoices [--export csv]*
54. **Table Extraction to Structured Rows** (P1) — Extracts tabular data natively from spreadsheets and via a Claude vision pass from PDF/image tables, normalizing into typed columns/rows with detected headers and source-cell provenance. — *grpc: AttachmentService.ExtractTables; mcp: extract_tables; cli: mail attach tables <msg>:<part>*
55. **Attachment Semantic Search** (P1) — Chunks and embeds extracted attachment text into a vector store fused with FTS5 via RRF, so "the contract clause about termination for convenience" returns the exact attachment and page. — *grpc: SearchService.SearchAttachments; mcp: search_attachments; cli: mail attach search "<query>"*
56. **Ask-Your-Attachment Q&A** (P1) — Answers a question scoped to one attachment or a search result set by retrieving relevant chunks and calling Claude, streaming an answer with page/section citations, refusing when context doesn't support it. — *grpc: AttachmentService.AskAttachment (stream); mcp: ask_attachment; cli: mail attach ask <msg>:<part> "<question>"*

## Analytics & Insights

57. **AI Periodic Digest** (P0) — A scheduled job clusters a window's mail by topic/sender and has Claude produce a ranked markdown briefing (needs-reply, FYI, waiting-on, auto-handled, skipped) with every line linked to source message-ids. — *grpc: AnalyticsService.GenerateDigest; mcp: generate_digest; cli: mail digest --since 7d [--deliver self]*
58. **Response-Time & SLA Analytics** (P0) — Pairs sent replies to their inbound message via In-Reply-To/References to compute per-contact/per-mailbox p50/p90 response times and a rolling trend, flagging where you are the bottleneck. — *grpc: AnalyticsService.GetResponseTimes; mcp: response_time_stats; cli: mail stats response-time --by contact*
59. **Contact Relationship Insights** (P1) — Aggregates per-contact volume, direction ratio, response symmetry, cadence, and topic history, then asks Claude for a one-paragraph relationship briefing and next actions, with a relationship-decay report. — *grpc: AnalyticsService.GetContactInsight; mcp: contact_insight; cli: mail contact <address> --insight*
60. **Newsletter & Subscription Detector** (P1) — Classifies senders as newsletters/automated/subscriptions using List-Unsubscribe headers, bulk heuristics, and a Claude fallback, tracking frequency/read-rate and producing an unsubscribe-candidates report with one-click execution. — *grpc: AnalyticsService.ListSubscriptions; mcp: list_subscriptions, unsubscribe; cli: mail subs [--unsubscribe <id>]*
61. **Natural-Language Analytics Query** (P1) — Claude translates a plain-English analytics question into a safe parameterized read-only SQL query over whitelisted views with row limits, returning both rows and a short narrative. — *grpc: AnalyticsService.AskAnalytics; mcp: ask_analytics; cli: mail ask "who did I ignore most last month?"*

## Integrations & Interop

62. **AI Priority Notification Engine** (P0) — On each new-mail event Claude Haiku scores every message into an importance tier plus a one-line reason, firing a macOS notification only at or above a per-account threshold so newsletters never ping. — *grpc: NotificationService.ScoreMessage/StreamAlerts; mcp: score_message, set_priority_threshold; cli: mail notify watch*
63. **Multi-Format Export (mbox/maildir/eml/json)** (P0) — Exports any query or thread to mbox, Maildir, .eml, or JSON, streaming from SQLite and preserving raw RFC822, with a `--with-ai` flag that batch-attaches Claude summaries and tags to the JSON. — *grpc: ExportService.Export; mcp: export_messages; cli: mail export 'from:alice' --format mbox -o out.mbox*
64. **Slack/Chat Forwarding with AI Summary Payloads** (P1) — A rule or manual action posts to a Slack/generic webhook with a Claude two-sentence summary, action items, and a deep link instead of the raw email, queued with retry and per-destination templates. — *grpc: ForwardService.SendWebhook/ListDeliveries; mcp: forward_message; cli: mail forward <id> --to slack:eng-alerts*
65. **Calendar & Task Extraction to External Tools** (P1) — Claude parses a message and any .ics into normalized events/tasks that can be written as .ics, piped to a command (Reminders/AppleScript), or POSTed to a task webhook, idempotent per message. — *grpc: ExtractService.ExtractEvents/ExtractTasks; mcp: extract_events, extract_tasks; cli: mail extract events <id> --format ics*
66. **URL & Link Extraction with AI Ranking** (P2) — Extracts and deduplicates all URLs from a message/thread and has Claude classify each (unsubscribe, tracking, meeting-link, document, CTA) with a relevance score, surfacing a picker that floats the highest-value link. — *grpc: LinkService.ExtractLinks; mcp: extract_links; cli: mail links <id> --open | --copy 2*

---
---

# PART III — DETAILED FEATURE SPECIFICATIONS

> The six features called out for depth. Each is implementable against the Rust stack. Search-related retrieval here (indexing, semantic layer) feeds the Part I pipeline; that pipeline is the authority on ranking.

---

## III-1. Fuzzy Finder

Universal `fzf`/`telescope`-style fuzzy finder. One prompt to jump anywhere: messages, folders, contacts, saved searches, tags, and commands. Local-only, never hits IMAP, instant on 100k+ messages. It is the **known-item / navigation** complement to full search (Part I): full search *ranks by relevance*; the finder *jumps by name*.

### Overview & Behavior

A single modal picker over a unified index of heterogeneous **items**. Item kinds: `message` (subject + sender + snippet), `mailbox` (path), `contact` (name + email), `saved_search` (named query), `tag` (label), `command` (palette action).

- Type → subsequence fuzzy match across all sources; results stream in ranked, best first.
- Preview pane updates on selection; `Enter` runs the item's default action.
- Multi-select + batch action for `message`/`contact`/`tag`.

Default action per kind:

```
message      -> open message
mailbox      -> switch to folder
contact      -> filter list to from/to contact
saved_search -> run query
tag          -> filter list to tag
command      -> execute command
```

### Scopes & Sigils

Opened in a **scope** that filters which kinds are searched (`all` default, or `messages`/`mailboxes`/`contacts`/`searches`/`tags`/`commands`/`in-folder`). Inline fzf-style sigils switch scope, stripped before matching:

```
>foo   commands      #foo   tags        @foo   contacts
/foo   saved_search  :foo   mailboxes   (no sigil → current scope)
```

### Fuzzy Match Algorithm

Skim/fzf-style **subsequence scoring** with bonuses, implemented in-crate (nucleo-style, no FFI). Query chars must appear in order (not necessarily contiguous); case-insensitive with **smart-case** (any uppercase → case-sensitive).

Per-matched-char score = base + bonuses:

```
base            +16   each matched char
consecutive     +8    matched char immediately follows previous match
word_boundary   +8    match at start of word
camelCase       +7    lower->Upper transition
after_separator +8    first alnum after / . _ - space @
first_char      +6    match at index 0
```

Penalties: `leading_gap -3` (cap -9), `gap -1` per unmatched between matches (cap -5). Prefer the alignment maximizing total score (bounded DP, O(query×candidate) with a byte-class table over pre-folded ASCII strings). Exact substring → flat `+40`, short-circuits DP. Empty query → rank by signals only. Returns `(score, positions)`; positions drive highlight.

### Blended Ranking

```
final = fuzzy
      + w_recency   * recency_decay(last_activity)   # exp(-age/half_life) scaled 0..64
      + w_unread    * is_unread
      + w_important * importance
      + w_frequency * interaction_count               # contacts/mailboxes
      + w_kind      * kind_priority(scope)            # command/mailbox outrank message for short queries
```

Ties: higher fuzzy → newer → shorter candidate → id. All weights TOML-tunable.

### Data Model

A denormalized, flattened in-SQLite index for all kinds, loaded to memory on startup and kept live via a change feed.

```sql
CREATE TABLE finder_index (
    item_id      INTEGER PRIMARY KEY,
    kind         INTEGER NOT NULL,        -- 0=message 1=mailbox 2=contact 3=saved_search 4=tag 5=command
    ref_id       INTEGER,                 -- fk into source table
    account_id   INTEGER, mailbox_id INTEGER,
    primary_text TEXT NOT NULL,           -- subject / path / name / label
    secondary    TEXT,                    -- sender / email / query
    snippet      TEXT,
    match_blob   TEXT NOT NULL,           -- lowercased ASCII-folded concat used for matching
    last_activity INTEGER, is_unread INTEGER DEFAULT 0,
    importance   REAL DEFAULT 0, frequency INTEGER DEFAULT 0, updated_at INTEGER NOT NULL
);
CREATE UNIQUE INDEX idx_finder_ref ON finder_index(kind, ref_id);

CREATE TABLE finder_dirty (               -- incremental change feed
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    kind INTEGER NOT NULL, ref_id INTEGER NOT NULL,
    op INTEGER NOT NULL,                  -- 0=upsert 1=delete
    created_at INTEGER NOT NULL
);

CREATE TABLE finder_commands (            -- command palette registry
    id INTEGER PRIMARY KEY, name TEXT NOT NULL, keywords TEXT,
    action TEXT NOT NULL, args_schema TEXT, context TEXT
);
```

Triggers on `messages`/`mailboxes`/`contacts`/`tags`/`saved_searches` write `finder_dirty` on insert/update/delete. A background task drains it every ~250 ms into an `Arc<RwLock<FinderStore>>` (`Vec<IndexEntry>` per kind, pre-folded `match_blob` inline). ~100 bytes + blob per entry → 100k messages ≈ 15–25 MB.

### Config

```toml
[finder]
enabled = true
default_scope = "all"
max_results = 200
snippet_max_bytes = 160
refresh_interval_ms = 250
smart_case = true
preview = true

[finder.ranking]
half_life_days = 30
w_recency = 40.0
w_unread = 25.0
w_important = 30.0
w_frequency = 10.0
w_kind = 15.0

[finder.keys]
open = "ctrl-p"
commands = "ctrl-shift-p"
multiselect = "tab"
```

### CLI

```
mail find "invoce acme"                    # all scopes
mail find --scope contacts "ali"
mail find --scope messages --in-folder Inbox "release notes"
mail find ">arch"                          # command scope via sigil
mail find "acme" --json                    # id\tkind\ttext + positions
mail find "acme" --select --action archive # batch action on all matches
mail find -i                               # interactive picker overlay
# piping (git/ripgrep vibe):
mail find "acme" --json | jq -r '.ref_id' | xargs -n1 mail open
```

### TUI

Modal floating overlay with prompt + scope badge, streaming result list, and a kind-specific preview pane. Matched chars highlighted from `positions`; kind glyphs (`@`/`#`/`:`/`>`). Keys: `ctrl-p`/`/` open, `tab` multi-select, `ctrl-a` select-all, `enter` default-or-batch action, `ctrl-p`/`alt-p` cycle scope, `ctrl-u/ctrl-d` preview scroll, `esc` dismiss. Results stream over an mpsc channel rendered per frame; matching runs on a worker — UI never blocks.

### gRPC Surface

Server-streaming so the client renders partial ranked results; a fresh keystroke cancels the prior stream.

```proto
service Finder {
  rpc Find(FindRequest) returns (stream FindResult);        // score-ordered batches
  rpc BatchAction(BatchActionRequest) returns (BatchActionResponse);
  rpc RebuildIndex(RebuildIndexRequest) returns (RebuildIndexResponse);
  rpc IndexStatus(IndexStatusRequest) returns (IndexStatusResponse);
}
enum ItemKind { ITEM_KIND_UNSPECIFIED=0; MESSAGE=1; MAILBOX=2; CONTACT=3; SAVED_SEARCH=4; TAG=5; COMMAND=6; }
message FindRequest { string query=1; Scope scope=2; int64 account_id=3; int64 mailbox_id=4; uint32 limit=5; bool with_positions=6; }
message FindResult { int64 item_id=1; ItemKind kind=2; int64 ref_id=3; int32 score=4; string primary_text=5; string secondary=6; string snippet=7; repeated uint32 positions=8; }
message BatchActionRequest { string action=1; repeated int64 ref_ids=2; map<string,string> params=3; }
```

Server matches into a bounded top-K heap, flushing descending-score batches (~every 2k candidates / 8 ms); client cancels on new keystroke, server aborts the scan.

### MCP Tool

`fuzzy_find { query, scope?, limit?, account?, in_folder? }` → ranked items (kind, ref_id, score, text, secondary, snippet). The agent's disambiguation primitive: resolve a fuzzy human reference ("the acme invoice") to concrete `ref_id`s, then chain `read_mail`/`archive_mail`/`draft_reply`. `fuzzy_batch_action` (nice-to-have) applies an action to a ref set.

### Claude / AI Integration

- **NL → scope + query:** prose input routes through `claude-haiku-4-5` to emit `{scope, query, filters}`; deterministic fuzzy still runs, Claude only rewrites.
- **Semantic fallback:** if top fuzzy score < threshold, augment with embedding kNN under a `w_semantic` weight (honors local-only policy).
- **Command palette NL:** unmatched prose in `commands` scope maps to the closest command + pre-filled args via Claude tool-selection using `args_schema`.
- **Preview enrichment:** lazy Claude one-line summary of the selected message, cached, never blocking.
- All AI is opt-in, off the hot path, time-boxed; the deterministic finder always returns first.

### Edge Cases & Performance

- Empty query → signal-ranked recents/frequent/all-commands. No matches → empty + optional "search semantically?" hint. Cold index → live FTS/LIKE fallback with an `indexing…` badge. Large dirty backlog → capped batched drain or targeted `RebuildIndex`. Stale ref → action returns `not_found`, entry pruned next drain. Unicode → NFKC + ASCII-fold for matching (`café` matches `cafe`), original preserved for display. Offline → fully functional.
- Target: **< 16 ms** to first batch, **< 50 ms** full ranked on 100k+ entries. Single-pass over in-memory `Vec<IndexEntry>` with a cheap "all query chars present" pre-filter before DP; per-kind vectors scanned only if in scope; bounded top-K heap; `rayon` shards for large kinds; ~20–30 ms keystroke debounce; `AtomicU64` query-generation cancellation. Memory < 25 MB for 100k messages.

---

## III-2. AI Auto-Processing of Incoming Mail (default Claude)

Every message that lands during background sync is automatically handed to AI (default = Claude) for deep analysis and enrichment. Output is stored locally, FTS- and embedding-indexed, and surfaced in TUI, CLI, gRPC (streaming), and MCP. Runs in the sync engine's process space, never blocks the TUI, fully offline-queued.

**Principles:** local-first (results cached forever, re-analysis opt-in) · cheap-by-default (Haiku triages, Opus goes deep only when warranted) · cost-bounded (hard token + dollar ceilings) · private (redaction + per-account opt-out before any byte leaves the machine).

### Pipeline

```
Sync Engine ──emits NewMessage(account, mailbox, uid)──▶ AI Queue (SQLite, persistent)
   │ dedupe, backpressure, rate/cost gate
   ▼ Worker Pool (tokio) : redact → build prompt → Claude Messages API (prompt cache)
   ▼ Result Writer : ai_summaries + ai_entities + ai_embeddings
   ▼ FTS5 index + embedding index + event bus (gRPC/MCP/TUI)
```

**Two passes:**

1. **Triage** (cheap, always) — `claude-haiku-4-5`. One structured JSON call: category, priority, `needs_reply`, sentiment, suggested tags, `tl_dr`.
2. **Deep** (opus, conditional) — `claude-opus-4-8`. Full summary, key points/TODOs, entity/date/amount extraction, thread-aware summary, suggested reply. Triggered when triage flags priority ≥ `high`, `needs_reply`, or category ∈ allowlist; otherwise skipped to save cost.

Thread-aware: the deep pass folds in prior `ai_summaries.summary` for the same `thread_id` (summaries, not full bodies), producing an incremental thread summary.

### What Claude produces (per message)

| Field | Type | Pass |
|---|---|---|
| `tl_dr` | one line | triage |
| `summary` | 2–4 sentences | deep |
| `key_points` | string[] | deep |
| `todos` | {text, due?, owner?}[] | deep |
| `entities` | {type, value, span}[] | deep |
| `dates` / `amounts` | normalized | deep |
| `sentiment` | positive/neutral/negative/urgent | triage |
| `category` | personal/work/newsletter/receipt/invoice/notification/spam/other | triage |
| `priority` | low/normal/high/critical | triage |
| `suggested_tags` | string[] | triage |
| `needs_reply` | bool | triage |
| `suggested_reply` | draft (nullable) | deep |
| `thread_summary` | incremental rollup | deep |

All structured output enforced via `output_config.format` (`json_schema`, `strict`) — no regex on model output.

### Data Model

```sql
CREATE TABLE ai_summaries (
    id INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL, thread_id TEXT,
    model TEXT NOT NULL, pass TEXT NOT NULL,          -- 'triage'|'deep'
    schema_version INTEGER NOT NULL,
    tl_dr TEXT, summary TEXT, thread_summary TEXT,
    key_points TEXT, todos TEXT,                       -- JSON arrays
    sentiment TEXT, category TEXT, priority TEXT,
    needs_reply INTEGER, suggested_reply TEXT, suggested_tags TEXT,
    input_tokens INTEGER, output_tokens INTEGER,
    cache_read_tokens INTEGER, cache_write_tokens INTEGER, cost_usd REAL,
    status TEXT NOT NULL DEFAULT 'ok', error TEXT, created_at INTEGER NOT NULL,
    UNIQUE(message_id, pass, model)
);
CREATE TABLE ai_entities (
    id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    kind TEXT NOT NULL, label TEXT, value TEXT NOT NULL,
    iso TEXT, amount REAL, currency TEXT, span_start INTEGER, span_end INTEGER
);
CREATE TABLE ai_embeddings (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    model TEXT NOT NULL, dim INTEGER NOT NULL, vector BLOB NOT NULL, created_at INTEGER NOT NULL
);
CREATE TABLE ai_queue (                                 -- persistent; survives restart/offline
    id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL, pass TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',              -- pending|leased|done|error|dead
    priority INTEGER NOT NULL DEFAULT 100, attempts INTEGER NOT NULL DEFAULT 0,
    lease_expiry INTEGER, next_attempt INTEGER NOT NULL DEFAULT 0, last_error TEXT,
    enqueued_at INTEGER NOT NULL, UNIQUE(message_id, pass)
);
CREATE TABLE ai_usage ( day TEXT PRIMARY KEY, requests INTEGER, input_tokens INTEGER, output_tokens INTEGER, cost_usd REAL );
```

FTS: an `ai_fts` FTS5 index over `tl_dr, summary, thread_summary, key_points, tags` (keyed by message rowid) — AI enrichments become first-class **search fields** (fed into the Part I lexical retriever with the `ai_summary` BM25 weight). New search filters: `ai:needs-reply`, `ai:priority>high`, `ai:category:invoice`, `ai:sentiment:negative`, `todo:`, `summary:<terms>`, `~semantic text`.

### Config

```toml
[ai]
enabled = true
provider = "claude"                # pluggable; "local" = offline path
api_key_command = "security find-generic-password -s anthropic -w"

[ai.models]
triage = "claude-haiku-4-5"
deep   = "claude-opus-4-8"          # or "claude-sonnet-5" (balanced)
embedding = "local"                 # local | claude  (default local for privacy)

[ai.deep_pass]
on_priority = "high"
on_needs_reply = true
categories = ["work", "personal", "invoice", "receipt"]
suggest_reply = true

[ai.limits]
max_concurrency = 4
requests_per_minute = 60
daily_token_cap = 2_000_000
daily_cost_cap_usd = 5.00
monthly_cost_cap_usd = 100.00
on_cap = "pause"                    # pause | triage_only | drop

[ai.batching]
enabled = true                      # Message Batches API (50% cost) for backlog
threshold = 200
max_batch = 5000

[ai.prompt_cache] enabled = true; ttl = "1h"
[ai.retry] max_attempts = 5; base_delay_ms = 1000; max_delay_ms = 60000

[ai.privacy]
redact = true
redact_patterns = ["ssn", "credit_card", "iban", "api_key", "otp"]
strip_attachments = true
max_body_chars = 40000

# Per-account override (hard opt-out possible)
[[accounts]]
name = "Personal-Legal"
[accounts.ai]
enabled = false                     # nothing leaves the machine
```

### CLI

```
mail ai status                      # queue depth, today's tokens/cost, headroom
mail ai process 123                 # force (re)analyze (deep pass)
mail ai process --since last-week   # backfill a range
mail ai summary 123 [--json]
mail ai reply 123 [--draft]         # suggested reply; --draft saves to Drafts
mail ai retry --failed
mail ai reindex --embeddings
mail ai pause | resume
mail ai cost --month
mail search "ai:needs-reply is:unread"
mail similar 123                    # embedding kNN neighbors
```

### TUI

Preview pane gains a collapsible **AI panel** rendered from local `ai_summaries` (instant, no network): TL;DR, key points, TODOs, "reply draft available (press R)". Keys: `A` toggle panel, `a` (re)analyze, `R` open suggested reply, `T` apply suggested tags, `zt` jump to next needs-reply, `o` cycle model for re-analysis. Status bar badge: `AI ⏳ 12 queued · $0.42 today`; pending rows show a spinner, failed rows `⚠`.

### gRPC Surface

```proto
service AiService {
  rpc GetSummary(GetSummaryRequest) returns (Summary);
  rpc AnalyzeMessage(AnalyzeRequest) returns (stream AnalyzeEvent);       // force + progress
  rpc StreamEnrichments(StreamEnrichmentsRequest) returns (stream Enrichment); // live feed
  rpc SuggestReply(SuggestReplyRequest) returns (Summary);
  rpc SemanticSearch(SemanticSearchRequest) returns (SemanticSearchResponse);
  rpc GetUsage(UsageRequest) returns (UsageStats);
  rpc SetPaused(SetPausedRequest) returns (Empty);
}
message Summary { int64 message_id=1; string thread_id=2; string model=3; string tl_dr=4; string summary=5; string thread_summary=6; repeated string key_points=7; repeated Todo todos=8; repeated Entity entities=9; string sentiment=10; string category=11; string priority=12; bool needs_reply=13; string suggested_reply=14; repeated string suggested_tags=15; Usage usage=16; string status=17; }
message Enrichment { int64 message_id=1; string pass=2; Summary summary=3; }
```

`StreamEnrichments` is the long-lived subscription an agent holds to watch the mailbox becoming smarter in real time (resume-from-cursor by `message_id`).

### MCP Tools

`summarize_message {message_id, force?, model?}` · `get_message_summary {message_id}` (cached only) · `list_needs_reply {account?, since?, limit?}` · `extract_entities {message_id, kind?}` · `semantic_search {query, k}` · `suggest_reply {message_id, tone?}` · `triage_inbox {account?, limit?}` · `ai_usage {}`. `summarize_message force=true` enqueues a deep pass and blocks on the result (respecting caps → `capped` error).

### Claude Integration Notes

Anthropic Messages API (`POST /v1/messages`) via `reqwest` + `rustls`. Structured output via `output_config.format` (`json_schema`, `strict`). Adaptive thinking on the deep pass only; triage thinking-off for latency. **Prompt caching:** frozen system prompt + stable JSON schema form the cached prefix (`cache_control: ephemeral, ttl 1h`); only the redacted body + prior thread summaries vary (uncached suffix). Verify via `usage.cache_read_input_tokens`; a warm request pre-warms the prefix at worker start. Cuts repeated-prefix input cost ~90% and TTFT.

**Queue/backpressure/cost:** persistent `ai_queue` (dedup via `UNIQUE(message_id, pass)`), lease model with expiry-reaping, `Semaphore(max_concurrency)` + token-bucket RPM limiter, cost gate against `ai_usage[today]` applying `on_cap`. **Batch mode:** when depth ≥ `threshold`, flip to the Message Batches API (`custom_id = message_id`, 50% cost) for initial-sync/offline-gap catch-up.

### Edge Cases & Performance

Offline → rows stay `pending`, drain on reconnect (nothing lost). Provider 429/5xx → exponential backoff w/ jitter, `attempts++`, then `dead`; `mail ai retry --failed` requeues. `refusal` → `status=error`, no retry. Empty-after-redaction → `redacted_skip`. Huge bodies → truncate at `max_body_chars`; attachments never uploaded. Duplicate UID / deleted mid-flight handled by `UNIQUE` + FK cascade. Schema drift tracked by `schema_version`. — AI runs off the sync/UI critical path (TUI reads local `ai_summaries` in <5 ms); two-pass keeps most mail on Haiku; batch API halves backlog cost; local embeddings = zero egress; SQLite-resident queue keeps memory flat (<150 MB).

---

## III-3. Deep Universal Indexing Engine ("index everything, really well")

A first-class indexing subsystem that indexes **everything** deeply so both humans and Claude can find anything instantly. It is the substrate the Part I ranking pipeline retrieves from.

Three cooperating layers over one message corpus, populated by a single **incremental, idempotent, resumable, background** pipeline:

- **Lexical** — SQLite FTS5 (BM25) over headers, bodies, attachments, notes, AI summaries.
- **Semantic** — chunk + embed messages/threads/attachments into a vector index; enables semantic + hybrid search and RAG.
- **Entities** — people, orgs, dates, amounts, links, tracking numbers, addresses, order/invoice IDs extracted and cross-linked.

Design: indexing is a **derived artifact** (safe to drop and rebuild from the message store); every unit of work is keyed by `(message_uid, index_kind, content_hash)` — re-running is a no-op unless content changed. Sync enqueues, the indexer drains, UI/CLI never block. Lexical is always on; semantic + entity degrade gracefully (offline / no key → lexical still works). Embeddings are **pluggable**: Voyage (Anthropic-ecosystem), local (fastembed/ONNX), or none.

### Pipeline

```
Sync ──enqueue──▶ index_queue (SQLite, durable) ──▶ Indexer workers (tokio, N)
   stages per message:  extract (text/OCR) → lexical (FTS5) → entities (NER/regex) → semantic (chunk+embed)
   each stage a separate index_kind → partial failure (e.g. embeddings API down) doesn't block the others
```

### Data Model (abridged)

```sql
CREATE TABLE index_content (        -- normalized, extracted text per part
  message_uid INTEGER NOT NULL, part TEXT NOT NULL,   -- subject/headers/body/attachment:<n>/note/summary
  mime TEXT, lang TEXT, text TEXT NOT NULL, chars INTEGER NOT NULL,
  content_hash BLOB NOT NULL, extracted_at INTEGER NOT NULL, extractor TEXT,
  PRIMARY KEY (message_uid, part)
);
CREATE VIRTUAL TABLE fts_messages USING fts5(
  subject, sender, recipients, body, attachments, notes, summary,
  content='', tokenize = "unicode61 remove_diacritics 2"      -- contentless; BM25 weights configurable
);
CREATE VIRTUAL TABLE vec_chunks USING vec0( chunk_id INTEGER PRIMARY KEY, embedding FLOAT[1024] );
CREATE TABLE chunks (
  chunk_id INTEGER PRIMARY KEY, message_uid INTEGER NOT NULL, thread_id INTEGER, part TEXT NOT NULL,
  seq INTEGER NOT NULL, text TEXT NOT NULL, tokens INTEGER NOT NULL, content_hash BLOB NOT NULL,
  model TEXT NOT NULL, dim INTEGER NOT NULL, embedded_at INTEGER NOT NULL
);
CREATE TABLE entities (
  entity_id INTEGER PRIMARY KEY, kind TEXT NOT NULL,   -- person/org/email/phone/url/amount/date/address/tracking_no/order_id/invoice_id/iban
  value TEXT NOT NULL, norm TEXT NOT NULL, meta TEXT, UNIQUE(kind, norm)
);
CREATE TABLE entity_mentions ( entity_id INTEGER, message_uid INTEGER, part TEXT, span_start INTEGER, span_end INTEGER,
  source TEXT, confidence REAL, PRIMARY KEY (entity_id, message_uid, part, span_start) );
CREATE TABLE entity_edges ( src_id INTEGER, dst_id INTEGER, rel TEXT, weight REAL DEFAULT 1.0, PRIMARY KEY (src_id, dst_id, rel) );
CREATE TABLE thread_index ( thread_id INTEGER PRIMARY KEY, root_uid INTEGER, subject_norm TEXT, participants TEXT,
  msg_count INTEGER, first_ts INTEGER, last_ts INTEGER, summary TEXT, summary_hash BLOB );
CREATE TABLE index_queue ( job_id INTEGER PRIMARY KEY, message_uid INTEGER, index_kind TEXT,   -- extract/lexical/entities/semantic/thread
  priority INTEGER DEFAULT 100, content_hash BLOB, state TEXT DEFAULT 'pending', attempts INTEGER DEFAULT 0,
  last_error TEXT, enqueued_at INTEGER, updated_at INTEGER, UNIQUE(message_uid, index_kind) );
CREATE TABLE index_state ( message_uid INTEGER, index_kind TEXT, content_hash BLOB, model TEXT, indexed_at INTEGER,
  PRIMARY KEY (message_uid, index_kind) );
```

Re-index decision = `index_queue.content_hash != index_state.content_hash` OR `index_state.model != config.embed_model`. Model/dim stored per-chunk so a model switch triggers targeted re-embed.

### Config

```toml
[index]
enabled = true
workers = 4
batch_size = 64                 # chunks per embed request
priority_recent_days = 30       # boost recent mail to front of queue
priority_mailboxes = ["INBOX"]

[index.lexical]
enabled = true
tokenizer = "unicode61 remove_diacritics 2"
weights = { subject = 8.0, sender = 4.0, recipients = 2.0, body = 1.0, attachments = 1.0, notes = 3.0, summary = 2.0 }

[index.extract]
strip_html = true
attachments = true
ocr = false                     # tesseract; opt-in
ocr_langs = ["eng"]
max_attachment_mb = 25
formats = ["pdf","docx","xlsx","pptx","txt","csv","eml"]

[index.semantic]
enabled = true
provider = "voyage"             # "voyage" | "local" | "none"
chunk_tokens = 512
chunk_overlap = 64
embed_threads = true
index_attachments = true
[index.semantic.voyage] model = "voyage-3"; dim = 1024; api_key_command = "security find-generic-password -s voyage -w"; rpm = 300
[index.semantic.local]  model = "bge-small-en-v1.5"; dim = 384    # fastembed / ONNX, offline

[index.entities]
enabled = true
regex = true                    # emails, phones, urls, amounts, tracking #s, IBANs
ner = "claude"                  # "claude" | "local" | "none"
ner_model = "claude-haiku-4-5"
min_confidence = 0.5

[index.search]
mode = "hybrid"                 # "lexical" | "semantic" | "hybrid"  (see Part I for full ranking)
rrf_k = 60
default_limit = 25
```

### CLI

```
mail index status                 # queue depth, coverage %, per-kind lag, model
mail index run                    # drain queue once (foreground, progress bar)
mail index start | stop
mail index reindex [--kind K] [--since DATE] [--mailbox M] [--message UID]
mail index rebuild --all          # full wipe + rebuild (confirm)
mail index verify                 # detect drift (state vs content_hash)
mail index gc                     # vacuum orphaned chunks/entities/fts rows
mail index embed --backfill       # embed anything missing vectors for current model
mail entities <kind> [--value V]  # e.g. mail entities tracking_no
mail entities links --entity <id> # entity_edges neighborhood
```

`mail index status` sample:

```
Coverage      lexical 100.0%   entities 98.2%   semantic 96.7%
Queue         412 pending   3 running   0 error
Model         voyage-3 (1024d)   chunks 1,284,551
Lag (p95)     new mail indexed < 4s after sync
```

### TUI

Ambient: a status-bar `idx ●` indicator (green/amber/red + queue depth); search box prefix toggles (`~` semantic, `=` exact); `g e` entity panel for the focused message (people/amounts/tracking #s/links, jump-to-thread); `Enter` on an entity → "find all mail mentioning this"; `g i` index-status view; `g a` ask-mailbox RAG prompt.

### gRPC Surface

```proto
service IndexService {
  rpc Status(StatusReq) returns (IndexStatus);
  rpc Reindex(ReindexReq) returns (stream IndexProgress);
  rpc Search(SearchReq) returns (SearchResults);
  rpc SearchStream(SearchReq) returns (stream SearchHit);        // early results
  rpc SemanticSearch(SemanticReq) returns (SearchResults);
  rpc Ask(AskReq) returns (stream AskEvent);                     // RAG: tokens + citations
  rpc Entities(EntityReq) returns (EntityResults);
  rpc EntityGraph(EntityGraphReq) returns (EntityGraph);
}
message AskEvent { oneof event { string token=1; Citation citation=2; RetrievalTrace trace=3; } }
```

### MCP Tools

`semantic_search {query, mode, limit, filter}` → hits with matched chunk text (ready to cite). `ask_mailbox {question, top_k, filter}` → `{answer, citations:[{message_uid, chunk_id, quote}]}` grounded on retrieved context only. `find_entities {kind, since?}` → normalized entities + linked messages. `mailbox_context {message|thread}` → assembled RAG bundle (chunks + entities + thread summary). All honor the same filter DSL as `search_mail`.

### Claude Integration & Hybrid Retrieval

- **Summaries feed the index** — Claude thread/message summaries written to `index_content(part='summary')` + `thread_index.summary`, indexed lexically **and** as their own semantic chunks (short, high-signal → excellent recall).
- **Entity NER** — optional `claude-haiku-4-5` pass augments regex, batched, cached by `content_hash`.
- **Embeddings** — `Embedder` trait (`model()/dim()/embed()`); default Voyage `voyage-3`, `provider="local"` fully offline.
- **RAG assembly** — retrieve via hybrid search → RRF → pack chunks under a token budget → prompt Claude (`claude-sonnet-5` default) with strict "cite message_uid" → stream tokens + citations. (Retrieval + rerank details in Part I.)

### Edge Cases & Performance

Embedding provider down → semantic jobs `pending` w/ backoff, lexical+entity proceed, search falls back to lexical. Model change → `index verify` flags stale, `index embed --backfill` re-embeds incrementally. Dimension change → versioned `vec_chunks_v2` + migration. Oversized/binary/encrypted attachments recorded with empty text, never block. Deleted/moved → tombstone cascade + orphan GC. Poison job quarantined after `max` attempts, never head-of-line blocks. Crash mid-batch → lease reclaim on startup, idempotent upserts.

Targets (100k msgs, M-series): lexical search < 50 ms · semantic top-k (1M chunks) < 120 ms · hybrid < 150 ms · new mail lexically indexed < 2 s / embedded < 10 s after sync · full cold reindex (local embed) < 30 min · `ask_mailbox` first token < 1.5 s. Tactics: batched embeds; WAL single-writer indexer + pooled read connections (search never blocks on writes); contentless FTS5; priority queue (recent/inbox first); `content_hash` short-circuit; optional int8 vector quantization for very large corpora.

---

## III-4. Notes & Tags

Attach freeform **notes** and arbitrary **tags** to messages and threads. Fully local, full-text indexed, keyboard-driven, exposed to Claude via MCP + gRPC. Tags round-trip to IMAP keywords / Gmail labels where the server allows; otherwise they live locally. Notes and tags are **first-class search fields** (see `note:`/`tag:` operators, Part I).

### Behavior

**Notes:** freeform **markdown** attached to a `message` or `thread`; multiple per target; timestamped, editable in `$EDITOR`; author `user` or `ai`; FTS5-indexed (`note:`); rendered in the preview pane; local-only. **Tags:** arbitrary colored labels (16-color + truecolor hex); optional `/` hierarchy (`project/alpha`); applied to message or thread (thread = all current + future members); fast filter + autocomplete; bulk tag/untag; per-tag sync mode (`local`/`imap`/`auto`). **AI:** on new mail Claude proposes tags (and optionally a summary note), pending until accepted, or auto-applied by rule above a confidence threshold.

### Data Model

```sql
CREATE TABLE tags (
    id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL, parent_id INTEGER REFERENCES tags(id) ON DELETE CASCADE,
    color TEXT, sync_mode TEXT NOT NULL DEFAULT 'auto',   -- 'local'|'imap'|'auto'
    imap_keyword TEXT, created_at INTEGER NOT NULL, UNIQUE(account_id, name)
);
CREATE TABLE message_tags (
    id INTEGER PRIMARY KEY, tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    message_id INTEGER REFERENCES messages(id) ON DELETE CASCADE,
    thread_id INTEGER REFERENCES threads(id) ON DELETE CASCADE,
    source TEXT NOT NULL DEFAULT 'user',                   -- 'user'|'ai'|'rule'|'imap'
    state TEXT NOT NULL DEFAULT 'applied',                 -- 'applied'|'pending'|'rejected'
    confidence REAL, created_at INTEGER NOT NULL,
    CHECK ((message_id IS NULL) <> (thread_id IS NULL)),
    UNIQUE(tag_id, message_id, thread_id)
);
CREATE TABLE notes (
    id INTEGER PRIMARY KEY,
    message_id INTEGER REFERENCES messages(id) ON DELETE CASCADE,
    thread_id INTEGER REFERENCES threads(id) ON DELETE CASCADE,
    body_md TEXT NOT NULL, author TEXT NOT NULL DEFAULT 'user',   -- 'user'|'ai'
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
    CHECK ((message_id IS NULL) <> (thread_id IS NULL))
);
CREATE VIRTUAL TABLE notes_fts USING fts5(body_md, content='notes', content_rowid='id', tokenize='unicode61 remove_diacritics 2');
CREATE TABLE tag_rules (
    id INTEGER PRIMARY KEY, account_id INTEGER REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL, query TEXT, ai_prompt TEXT,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    mode TEXT NOT NULL DEFAULT 'suggest',                  -- 'suggest'|'auto'
    min_conf REAL DEFAULT 0.75, enabled INTEGER NOT NULL DEFAULT 1, created_at INTEGER NOT NULL
);
```

**Effective tags** = a message's own `message_tags` ∪ its thread's `message_tags` (a `messages_tags_effective` view backs list rendering + search). Triggers keep `notes_fts` in sync.

### Config

```toml
[tags]
palette = ["#7aa2f7", "#e0af68", "#9ece6a", "#f7768e", "#bb9af7", "#7dcfff"]
hierarchy_separator = "/"
default_sync_mode = "auto"
[tags.imap] keyword_prefix = "rmail/"; map_system = true; gmail_labels = true
[notes] editor = "$EDITOR"; preview_lines = 6; index = true
[tags.ai]
enabled = true
model = "claude-haiku-4-5"
suggest_on_new_mail = true
max_suggestions = 3
auto_apply_min_confidence = 0.85
taxonomy = ["work","personal","finance/invoice","finance/receipt","travel","newsletter","urgent","follow-up"]
```

### CLI

```
mail tag <id|thread:<tid>|search:<query>> <tag> [<tag>...]   # add (message/thread/bulk)
mail untag <id> <tag>
mail tag <id> --thread work
mail tags                                  # list all + counts
mail tags create project/alpha --color '#7aa2f7' --sync imap
mail tags rename project/alpha project/beta
mail tags delete newsletter
mail tag --bulk search:"from:stripe" finance/receipt
mail note <id>                             # $EDITOR
mail note <id> -m "quick note"
mail note <id> --thread
mail notes <id> ; mail note edit <note_id> ; mail note rm <note_id>
mail suggest-tags <id> [--apply]
mail accept-tags <id> [<tag>...] ; mail reject-tags <id> [<tag>...]
mail rules list | add | rm
```

Search operators (extend the Part I grammar): `tag:work`, `tag:project/*`, `-tag:newsletter`, `note:invoice`, `has:note`, `has:tag`, composable with everything else.

### TUI

Preview pane shows a **Tags** chip row (colored) + **Notes** section (markdown, newest first, collapsed to `preview_lines`); pending AI tags render dimmed with `?` and `[a]ccept [x]reject`. Keys: `t` tag palette (fuzzy add), `T` tag thread, `u` untag, `n`/`N` add note to message/thread, `e` edit note, `dd` delete note, `g t` Claude suggest-tags (async → pending), `a`/`x` accept/reject suggestion, `V` visual-select → bulk `t`/`u`, `f t` filter by tag. Tag palette fuzzy-matches existing tags, autocompletes hierarchy, `Enter` on a new name → create-then-apply. Tagging is optimistic: instant local write, IMAP keyword push queued and reconciled in background.

### gRPC & MCP

```proto
service Tagging {
  rpc AddTag(AddTagRequest) returns (TagApplication);
  rpc RemoveTag(RemoveTagRequest) returns (Empty);
  rpc ListTags(ListTagsRequest) returns (ListTagsResponse);
  rpc CreateTag(CreateTagRequest) returns (Tag);
  rpc BulkTag(BulkTagRequest) returns (BulkTagResponse);              // query or id list
  rpc SuggestTags(SuggestTagsRequest) returns (stream TagSuggestion); // streamed as Claude responds
  rpc ResolveSuggestion(ResolveSuggestionRequest) returns (Empty);   // accept|reject
}
service Notes {
  rpc AddNote(AddNoteRequest) returns (Note);
  rpc EditNote(EditNoteRequest) returns (Note);
  rpc DeleteNote(DeleteNoteRequest) returns (Empty);
  rpc ListNotes(ListNotesRequest) returns (ListNotesResponse);
  rpc WatchNotes(WatchNotesRequest) returns (stream NoteEvent);      // live add/edit/delete
}
message Target { oneof of { uint64 message_id = 1; uint64 thread_id = 2; } }
```

MCP tools (thin wrappers): `add_note {target, body_md, author?}` (author defaults `ai` when Claude calls) · `list_notes {target}` · `add_tag {target, tags[], source?}` · `remove_tag {target, tag}` · `suggest_tags {message_id, apply?}` (classifies against taxonomy, writes `pending` unless `apply`). Responses include subject/id so agents chain `search_mail` → `suggest_tags` → `add_tag`.

### Claude Integration

**Suggestion pipeline:** new message → low-priority `suggest_tags` job → `claude-haiku-4-5` structured JSON `[{tag, confidence, rationale}]` → write `message_tags(state='pending', source='ai')` → rules pass promotes `mode='auto'` above `min_conf` → rest stay pending for accept/reject. **Auto-tag rules:** deterministic (`query`) apply immediately; AI-scored (`ai_prompt` + `min_conf`) apply over threshold. `summarize_thread` can persist output as an `author='ai'` note (FTS-searchable). Cost control: batch new-mail classification, skip already-user-tagged mail, respect the global confidence ceiling.

### IMAP/Gmail Interop & Edge Cases

`sync_mode=imap` maps tag ⇄ IMAP keyword (`STORE +FLAGS (rmail/<name>)`) or Gmail `X-GM-LABELS`; `local` never touched by sync; `auto` attempts imap then downgrades to local on `NO`/unsupported. Inbound server keywords/labels import as `source='imap'` tags. System flags (`\Flagged`, `$Important`) map to reserved built-ins when `map_system=true`. Message move → tags/notes follow stable `messages.id`, only the keyword re-`STORE`d. IMAP push failure → local is source of truth, re-queued with backoff, persistent failure → auto-downgrade + warn. Duplicate application idempotent via `UNIQUE`. Concurrent note edit → last-write-wins on `updated_at`, `WatchNotes` refreshes open UIs. Hierarchy cycles rejected. Performance: tag filtering index-backed (<50 ms); AI suggestion fully off the hot path; bulk tag = single transaction + coalesced IMAP `STORE` UID sets.

---

## III-5. Send Later / Scheduled Send

Compose now, deliver later. Every outgoing message may carry a `send_at`; it lives in a local **outbox** until the scheduler fires, then goes over SMTP. Works across restarts, offline windows, and timezones. Also covers undo-send, cancel/edit, follow-up reminders, AI-suggested optimal send time, and "Claude drafts → user schedules". **Guiding principle:** nothing depends on rmail running at the exact `send_at` instant beyond a small tolerance — a late start still sends, never silently drops.

### Behavior

- **Scheduled send:** absolute time or natural language → serialized to full RFC 5322 MIME, stored in `outbox`; scheduler wakes at `send_at`, transmits via SMTP, appends to IMAP `Sent`.
- **Undo send:** every send is really "schedule at `now + undo_window`" (default 10s for immediate); one keypress/API call cancels within the window; window 0 = true immediate.
- **Offline:** scheduler runs regardless; overdue-but-unreachable stays `scheduled` with `next_attempt_at`, retried with backoff; not `failed` due purely to being offline until `max_retries`.
- **Timezone:** times stored UTC epoch; a `tz` column keeps the IANA zone scheduled in; natural language resolved against the account's `default_timezone` else system local, then frozen to an absolute instant.

### Outbox Lifecycle

```
scheduled → sending → sent
scheduled → sending → scheduled   (transient failure, retry)
scheduled → sending → failed      (permanent / retries exhausted)
scheduled → canceled              (user)
```

`sending` is leased (`lease_expires_at`); a crash mid-send resets an expired lease to `scheduled` (unless a delivery receipt was persisted — see idempotency).

### Data Model

```sql
CREATE TABLE outbox (
    id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id),
    from_addr TEXT NOT NULL, to_addrs TEXT NOT NULL, cc_addrs TEXT, bcc_addrs TEXT, subject TEXT,
    raw_mime BLOB NOT NULL,                              -- full RFC 5322, source of truth
    body_preview TEXT, in_reply_to TEXT, thread_id INTEGER,
    send_at INTEGER NOT NULL, tz TEXT NOT NULL DEFAULT 'UTC',
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'scheduled',
    origin TEXT NOT NULL DEFAULT 'user',                -- user|ai|followup|undo
    attempts INTEGER NOT NULL DEFAULT 0, max_retries INTEGER NOT NULL DEFAULT 5,
    next_attempt_at INTEGER, lease_expires_at INTEGER, last_error TEXT,
    smtp_message_id TEXT,                               -- Message-ID emitted (idempotency)
    sent_at INTEGER, undo_deadline INTEGER
);
CREATE INDEX idx_outbox_due ON outbox(state, send_at);
CREATE TABLE followups (
    id INTEGER PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id),
    thread_id INTEGER NOT NULL, message_id TEXT NOT NULL,
    remind_at INTEGER NOT NULL, tz TEXT NOT NULL DEFAULT 'UTC',
    cancel_on_reply INTEGER NOT NULL DEFAULT 1,
    state TEXT NOT NULL DEFAULT 'armed',                -- armed|fired|dismissed
    note TEXT, created_at INTEGER NOT NULL
);
```

### Config

```toml
[send]
undo_window = "10s"                # 0 disables undo-send
default_timezone = "America/Los_Angeles"
poll_interval = "30s"
late_tolerance = "10m"
max_retries = 5
backoff_base = "30s"
backoff_max = "30m"
append_to_sent = true
ai_requires_confirmation = true    # MCP-originated sends always get an undo window

[send.optimal]
enabled = true
model = "claude-haiku-4-5"
earliest = "08:00"
latest = "18:00"
prefer_recipient_tz = true

[send.followup] default_delay = "3d"; cancel_on_reply = true
```

### CLI

```
mail send --to alice@x.com --subject "Hi" --at "2026-07-26T09:00:00-07:00"
mail send --to alice@x.com --at "tomorrow 9am" --body-file draft.txt
mail send --draft 42 --at "next monday 8:30am"
mail send --draft 42 --at optimal              # AI-chosen time
mail send --to alice@x.com --at "recipient 9am"
mail send --to alice@x.com --subject "Oops" --body "..."   # immediate, undoable
mail undo [<outbox_id>]
mail outbox [--state failed]
mail outbox show <id> ; mail outbox cancel <id>
mail outbox reschedule <id> --at "friday 5pm"
mail outbox edit <id> ; mail outbox retry <id> ; mail outbox send-now <id>
mail followup <message_id> --in "3d" --note "chase quote"
mail followup list ; mail followup dismiss <id>
```

### TUI

New **Outbox** pseudo-folder above Sent, badged with `scheduled + failed` counts. Compose footer: `[C-s] Send   [C-l] Send Later   [C-t] Optimal time   [C-u] Undo window`. `C-l` opens a time picker (presets + custom NL input echoing the resolved absolute time live + Optimal-AI suggestion). After an immediate send, an undo toast counts down (`press [u] to undo (9s)`). Outbox view keys: `Enter` preview, `e` edit, `r` reschedule, `x` cancel, `s` send now, `R` retry; failed rows show `last_error`.

### gRPC & MCP

```proto
service SendScheduler {
  rpc ScheduleSend(ScheduleSendRequest) returns (OutboxEntry);       // schedule or immediate-with-undo
  rpc CancelScheduled(CancelRequest) returns (OutboxEntry);
  rpc RescheduleSend(RescheduleRequest) returns (OutboxEntry);
  rpc UpdateScheduledBody(UpdateBodyRequest) returns (OutboxEntry);
  rpc SendNow(IdRequest) returns (OutboxEntry);
  rpc RetryFailed(IdRequest) returns (OutboxEntry);
  rpc ListOutbox(ListOutboxRequest) returns (ListOutboxResponse);
  rpc WatchOutbox(WatchOutboxRequest) returns (stream OutboxEvent);  // every state transition
  rpc SuggestSendTime(SuggestSendTimeRequest) returns (SuggestSendTimeResponse);
  rpc CreateFollowup(CreateFollowupRequest) returns (Followup);
  rpc ListFollowups(ListFollowupsRequest) returns (ListFollowupsResponse);
  rpc DismissFollowup(IdRequest) returns (Followup);
}
message ScheduleSendRequest {
  int64 account_id=1; optional int64 draft_id=2;
  repeated string to=3; repeated string cc=4; repeated string bcc=5;
  optional string subject=6; optional string body=7; optional string in_reply_to=8;
  optional int64 send_at=9; optional string send_at_nl=10; optional bool optimal=11;
  optional string recipient_local=12; string tz=13; optional int64 undo_window_secs=14; string origin=15;
}
```

MCP tools: `schedule_send {account, to[], subject, body, send_at, timezone}` (`send_at` accepts ISO-8601, natural language, `"optimal"`, or `"recipient 9am"`; returns the resolved `OutboxEntry`) · `list_outbox {state?, account?}` · `cancel_scheduled {id}` · `suggest_send_time {…}` (no side effects — propose then `schedule_send`) · `create_followup {message_id, remind_in, note?}`. **Safety:** MCP-originated sends store `origin="ai"` and are always subject to the undo window so a human can intercept (`ai_requires_confirmation`).

### Claude Integration

- **Draft → schedule:** Claude drafts via `draft_reply`, then calls `schedule_send` instead of sending ("reply to this, but send it tomorrow morning").
- **Optimal time:** `claude-haiku-4-5` given recipient/domain, inferred recipient timezone (from prior `Date` header offsets in the local thread, or heuristics), the sender's reply-time history, and `earliest`/`latest` guardrails → constrained JSON `{send_at, tz, rationale, alternatives}`, clamped to guardrails.
- **Recipient-tz inference:** explicit zone → recent `Date` header offset → AI/domain heuristic → sender default (low-confidence flag).
- **NL parsing:** deterministic `chrono` grammar first; Claude only for ambiguous input, always echoing the resolved absolute time for confirmation.

### Edge Cases & Performance

Missed window (rmail off): on startup send if within `late_tolerance`, else send but mark "sent late (was offline)" — never drop. Crash mid-send: expired leases reset; **idempotency** via `smtp_message_id` persisted before SMTP `DATA` (retry treats an already-present Message-ID as `sent` → at-most-once). SMTP transient (4xx/offline) → backoff, stay `scheduled`; permanent (5xx/auth/invalid recipient) → `failed`. Cancel/undo enforced transactionally (`scheduled → canceled`; after deadline returns `AlreadySent`). DST: absolute instant frozen at schedule time, `tz` retained for display only. Scheduler is woken on wall-clock + wake-from-sleep + network-up events (not just a poll timer). `append_to_sent` strips Bcc from the appended copy. Follow-up auto-dismiss on detected reply. — Cost: single indexed due-query on `idx_outbox_due`; no busy-polling (sleeps until `min(next_due, poll_interval)`, woken by `Notify` on insert); bounded SMTP worker pool (default 2), per-account connection reuse; `WatchOutbox` fans out via an in-process broadcast channel.

---

## III-6. Full gRPC API (complete programmatic surface for EVERY feature)

rmail exposes **one core API** over gRPC (tonic). Every capability — accounts, sync, mail, search, index, tags, notes, compose/outbox, AI, automation, admin — is an RPC. The CLI, TUI, and MCP server are all **thin clients** of it.

> **If the CLI can do it, gRPC can do it. If gRPC can't do it, it isn't a feature.**

The gRPC server runs inside `rmaild`, which owns background sync, indexing, ranking, AI, and the SQLite database. Clients never touch SQLite or IMAP directly.

**Transports:** Unix domain socket (default, local, trusted, peer-uid auth) · TCP (optional, token or mTLS) · gRPC-web (optional, browsers, via tonic-web). **Server features:** reflection (`grpc.reflection.v1`), health checks (`grpc.health.v1`), server-streaming (progress/events/tokens), bidi streaming (interactive AI chat).

### Service Decomposition

| Service | Responsibility |
|---|---|
| `AccountService` | CRUD accounts, test connection, OAuth, credential status |
| `SyncService` | trigger/stream sync, status, backfill, pause/resume, IDLE watch |
| `MailService` | list/get/move/copy/flag/delete, threads, attachments, **WatchEvents** |
| `SearchService` | the Part I ranking pipeline: search, semantic, compile, explain, feedback |
| `IndexService` | FTS/vector/entity index build + status |
| `Finder` | fuzzy finder (Part III-1) |
| `TagService` / `NoteService` | tags & notes CRUD |
| `ComposeService` / `SendScheduler` (OutboxService) | drafts + scheduled send/outbox |
| `AiService` | summarize, triage, draft, extract, **AskMailbox (RAG)**, **Chat (bidi)** |
| `AutomationService` | rules/hooks/webhooks CRUD, dry-run, run-now, log |
| `AiPolicyService` / `AuditService` | redaction, residency, budgets, audit ledger |
| `AdminService` | daemon status, config get/set, vacuum, reindex, tokens, version, shutdown |

Core service implementations are plain Rust; tonic handlers, clap (CLI), and MCP are adapters over them. MCP holds an in-process channel — no extra socket hop locally.

### Data Model (API bookkeeping)

```sql
CREATE TABLE api_tokens (
    id INTEGER PRIMARY KEY, name TEXT NOT NULL, token_hash BLOB NOT NULL,  -- argon2id
    scopes TEXT NOT NULL,                          -- "mail.read,mail.send,ai.invoke,ai.spend:5,admin"
    created_at INTEGER NOT NULL, last_used_at INTEGER, expires_at INTEGER, revoked INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE idempotency_keys (
    key TEXT PRIMARY KEY, method TEXT NOT NULL, request_hash BLOB NOT NULL,
    response BLOB, status_code INTEGER NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL
);
CREATE TABLE events (                              -- durable log powering WatchEvents (resumable)
    seq INTEGER PRIMARY KEY AUTOINCREMENT, account_id INTEGER, kind TEXT NOT NULL,
    message_id INTEGER, payload BLOB NOT NULL, created_at INTEGER NOT NULL
);
```

### Config

```toml
[grpc]
enabled = true
socket_path = "~/.local/state/rmail/rmaild.sock"   # 0600, owner-only
listen = ""                                        # e.g. "127.0.0.1:50551"
tcp_enabled = false
auth = "token"                                     # "none" | "token" | "mtls"
[grpc.tls] cert_file = ""; key_file = ""; client_ca = ""
[grpc.web] enabled = false; cors_origins = ["http://localhost:5173"]
[grpc.limits] max_message_bytes = 16_777_216; max_concurrent = 256; stream_buffer = 1024; request_timeout_secs = 120
[grpc.events] retention_days = 7; retention_rows = 1_000_000
```

Unix socket is always available when `grpc.enabled`; TCP requires `tcp_enabled` AND (`auth != "none"` OR `--insecure`).

### CLI (daemon + generic client)

```
rmail daemon start | status | stop
rmail api token create --name ci --scopes mail.read,ai.invoke --ttl 90d
rmail api token list | revoke <id>
rmail api ping                                     # Health.Check round-trip + latency
rmail api reflect                                  # list services/methods via reflection
rmail api call MailService.List '{"mailbox":"INBOX","page_size":50}'   # generic
```

Global flags: `--socket <path>`, `--addr <host:port>`, `--token <token>` (or `RMAIL_TOKEN`), `--tls-ca/--tls-cert/--tls-key`, `--insecure`, `--deadline <secs>`.

### Representative RPCs

```proto
syntax = "proto3";
package rmail.v1;
import "google/protobuf/timestamp.proto";

service MailService {
  rpc List(ListMessagesRequest) returns (stream Message);       // stream large pages
  rpc Get(GetMessageRequest) returns (FullMessage);
  rpc GetThread(GetThreadRequest) returns (Thread);
  rpc Move(MoveRequest) returns (Empty);
  rpc Copy(CopyRequest) returns (Empty);
  rpc SetFlags(SetFlagsRequest) returns (Empty);
  rpc Delete(DeleteRequest) returns (Empty);
  rpc GetAttachment(GetAttachmentRequest) returns (stream AttachmentChunk);
  // KEY: unified live feed — new mail, flag/move/delete, sync state, send results, rule fires, AI summaries.
  rpc WatchEvents(WatchEventsRequest) returns (stream Event);
}
message WatchEventsRequest { int64 account_id=1; int64 since_seq=2; repeated EventKind kinds=3; bool include_ai_summary=4; }
message Event {
  int64 seq=1; EventKind kind=2; google.protobuf.Timestamp at=3;
  oneof body { Message new_mail=4; FlagChange flag=5; MoveEvent moved=6; int64 deleted_id=7;
               SyncStatus sync=8; SendResult send=9; RuleFired rule=10; AiSummary ai_summary=11; }
}
enum EventKind { EVENT_KIND_UNSPECIFIED=0; NEW_MAIL=1; FLAG_CHANGED=2; MOVED=3; DELETED=4; SYNC_STATE=5; SEND_RESULT=6; RULE_FIRED=7; AI_SUMMARY=8; }

service AiService {
  rpc Summarize(SummarizeRequest) returns (stream Token);       // token streaming
  rpc Triage(TriageRequest) returns (TriageResult);
  rpc Draft(DraftRequest) returns (stream Token);
  rpc Extract(ExtractRequest) returns (ExtractResult);
  rpc AskMailbox(AskRequest) returns (stream AskChunk);         // KEY: RAG over local mail
  rpc Chat(stream ChatClientMsg) returns (stream ChatServerMsg); // KEY: interactive mailbox chat (bidi)
}
message AskChunk { oneof body { Token token=1; Citation citation=2; Usage usage=3; } }
message ChatServerMsg { oneof body { Token token=1; ToolCall tool_call=2; Citation citation=3; Usage usage=4; } }

service AdminService {
  rpc GetStatus(Empty) returns (DaemonStatus);
  rpc GetConfig(Empty) returns (ConfigDoc);
  rpc SetConfig(SetConfigRequest) returns (ConfigDoc);          // hot-reload-safe keys
  rpc MintToken(MintTokenRequest) returns (Token);
  rpc RevokeToken(RevokeTokenRequest) returns (Empty);
  rpc Vacuum(Empty) returns (Empty);
  rpc Reindex(ReindexRequest) returns (stream IndexProgress);
  rpc Version(Empty) returns (VersionInfo);
  rpc Shutdown(ShutdownRequest) returns (Empty);
}
```

**Streaming map:** unary (Get, SetFlags, Move, Triage, Extract, GetStatus) · server-stream (Sync, Search, WatchEvents, Summarize, AskMailbox, Send, Build) · bidi (`AiService.Chat`).

### Auth & Scopes

**Unix socket:** perms `0600`, peer uid via `SO_PEERCRED` → implicit `admin`; the trusted local path for CLI/TUI/MCP. **TCP token:** `authorization: Bearer <token>` → argon2id constant-time verify → scope check. **mTLS:** client cert vs `client_ca`, CN → scopes. **Scopes:** `mail.read`, `mail.write`, `mail.send`, `ai.invoke`, `ai.spend:<usd>`, `mailbox:<name>`, `automation`, `admin`; a tonic interceptor enforces the required scope per method — so you can hand Claude a token that reads and summarizes freely but cannot send or delete.

### Relationship to MCP

**MCP tools are thin wrappers over the gRPC services — one core API, two front doors**, auto-projected from the compiled descriptor set + per-RPC annotations. `search_mail`→`SearchService.Search`, `read_mail`→`MailService.Get`, `draft_reply`→`ComposeService.CreateDraft`, `send_mail`→`OutboxService.Enqueue`+`Send`, `summarize_thread`→`AiService.Summarize`, `ask_mailbox`→`AiService.AskMailbox`. A new gRPC RPC becomes an MCP tool with zero extra code → no gRPC/MCP drift.

### Error Model, Versioning, Idempotency

Rich `google.rpc.Status` + `ErrorInfo` (stable `reason` enum; clients branch on `reason`, never message text): `UNAUTHENTICATED`, `PERMISSION_DENIED` (scope), `NOT_FOUND`, `FAILED_PRECONDITION` (daemon offline / not synced / no AI key), `UNAVAILABLE` (IMAP/SMTP unreachable → retry), `RESOURCE_EXHAUSTED` (rate/size/budget), `DEADLINE_EXCEEDED`, `ALREADY_EXISTS` (idempotency replay w/ differing payload). Package `rmail.v1`; additive-only within a major, breaking → `rmail.v2` served alongside ≥1 release. Pagination: `page_size` (server caps 500) + opaque `page_token`. Idempotency: mutating RPCs carry `idempotency_key` (UUID) — same key+hash replays the cached response, critical for `Send`/`Move`/`Delete`.

### Client SDKs & Edge Cases

Single `.proto` set is the source of truth; `buf` for lint/breaking-change CI. SDKs: Rust (`tonic-build`, used internally), Python (`grpcio-tools`), TypeScript (`ts-proto`/Connect for gRPC-web), Go, and `grpcurl` via reflection with zero SDK. — Daemon not running → CLI auto-starts `rmaild` or errors `FAILED_PRECONDITION`; TUI shows an offline banner + backoff reconnect. Stream disconnect → resume `WatchEvents` via `since_seq`; retention gap → `OUT_OF_RANGE` with `oldest_seq` → client resyncs. Event backpressure → bounded per-stream channel, overflow drops to cursor resume rather than blocking sync. AI down → `AiService` health `NOT_SERVING`, mail features unaffected. Large attachments chunk-streamed within the 16 MiB frame cap. Performance: zero-copy in-process channel for MCP; first `SearchHit`/AI `Token` in <50 ms/provider-latency (never buffer whole result sets); bounded channels + concurrency cap; SQLite WAL read-pool for parallel reads; daemon within the <150 MB target.

# PART IV — CROSS-CUTTING SYSTEMS

## AI Integration (Claude)

One AI layer serves every feature. Default provider **Claude** via the Anthropic Messages API; a `Provider` trait allows alternates and a fully-local path.

- **Models:** `claude-opus-4-8` (deep summaries, hard reasoning), `claude-sonnet-5` (balanced default for RAG/drafting), `claude-haiku-4-5` (high-volume triage, tagging, classification, rerank). Per-feature and per-account overridable.
- **Structured output** via `output_config.format` (`json_schema`, `strict`) everywhere JSON is consumed — no regex on model output.
- **Streaming** — Messages API SSE deltas map 1:1 to gRPC `stream Token` frames; upstream request aborted on client cancel/deadline.
- **Prompt caching** — frozen system prompt + stable schema form a cached prefix; only per-message content varies. ~90% input-cost cut on repeated prefixes.
- **Batch API** — backlog/initial-sync catch-up uses Message Batches (50% cost, `custom_id = message_id`).
- **Embeddings** — pluggable; local (fastembed/ONNX, default for privacy) or Voyage; power the semantic retriever and RAG.
- **Cost governance** — token/dollar budgets (§ Security), soft-cap model downgrade (opus→sonnet→haiku), hard-cap block, audit ledger.

## MCP Server (expanded)

Auto-projected from the gRPC services (§ III-6). Tools grouped:

- **Read/search:** `search_mail` (ranked, the primary tool), `semantic_search`, `hybrid_search`, `read_mail`, `recent_mail`, `unread_mail`, `similar_messages`, `explain_ranking`, `fuzzy_find`.
- **AI over mail:** `summarize_thread`, `summarize_message`, `ask_mailbox` (RAG w/ citations), `triage_inbox`, `extract_data`, `extract_entities`, `extract_invoice`, `extract_events`/`extract_tasks`, `run_prompt`, `get_memory`/`add_memory`.
- **Organize:** `add_note`/`list_notes`/`draft_note`, `add_tag`/`remove_tag`/`suggest_tags`, `create_smart_folder`, `mute`, `bulk_preview`/`bulk_apply`.
- **Compose/send:** `draft_reply`, `rewrite_draft`, `preflight_check`, `schedule_send`, `list_outbox`, `cancel_scheduled`, `undo_send`, `suggest_send_time`, `send_mail`, `archive_mail`, `delete_mail`, `save_attachment`.
- **Automation/insight:** `create_rule`/`synthesize_rule`/`backtest_rule`, `run_inbox_agent`, `register_webhook`, `generate_digest`, `contact_insight`, `list_subscriptions`/`unsubscribe`, `ask_analytics`.
- **Ops/safety:** `subscribe_events`, `index_status`, `ai_usage`, `get_ai_budget`, `redact_preview`, `evaluate_ai_policy`, `scan_prompt_injection`, `query_ai_audit`.

Mutating tools are gated by capability-token scope; an agent with a read-only token sees only read tools.

## Automation, Rules & Hooks

- **Rules** — TOML predicates mixing deterministic matchers (`from`/`subject`/`header`/`flags`/`size` regex) with a `claude_is` NL predicate; actions `move/label/flag/archive/notify/run-hook/draft-reply`; classification cached by `message-id + prompt-hash`; evaluated on each new message. NL rule synthesis + dry-run backtest with per-decision Claude explanations.
- **Autonomous inbox agent** — a bounded, allowlisted, dry-run-by-default agentic loop that triages toward a policy, every action logged with its reason.
- **Hooks** — shell commands on events (`on_new_message`, `on_label`, `on_move`, `on_rule_match`, `on_sync_error`), event JSON on stdin, bounded worker pool + timeouts.
- **Webhooks** — HMAC-signed, retried, persisted delivery queue; payloads optionally carry a Claude summary + extracted fields.

## Security, Privacy & AI Safety

- **Credentials** — never plaintext; macOS Keychain, password command, or env var; OAuth2 refresh tokens in Keychain.
- **PII Redaction Firewall** — mandatory pre-flight before any Claude call: reversibly tokenizes emails/phones/cards(Luhn)/addresses/secrets/names in memory, re-hydrates the response — the API never sees raw PII.
- **AI Policy & Data-Residency** — declarative per-account/folder/pattern `allowed | local-only | forbidden` + residency tag; forbidden folders are invisible to AI; every decision logged and `explain`-able.
- **Budget enforcer** — per-account + global daily/monthly token & dollar caps; soft-cap model downgrade, hard-cap block; bulk jobs sub-budgeted.
- **Prompt-injection shield** — email is attacker-controlled: bodies wrapped in untrusted-content delimiters, scanned for injection patterns/hidden text/zero-width chars; flagged messages require confirmation before any AI action.
- **Audit ledger** — append-only record of every Claude call (model, tokens, cost, redaction level, latency, SHA-256 of the exact payload sent); every AI artifact links to its ledger entry.
- **Local-only path** — on-device generation + embeddings for accounts that may never reach the cloud.
- **Capability tokens** — least-privilege scopes on every gRPC/MCP call (§ III-6).

## Analytics & Insights

AI periodic digests (prioritized, source-linked briefings), response-time/SLA analytics (In-Reply-To pairing → p50/p90, bottleneck flagging), contact relationship insights + decay reports, newsletter/subscription detection with one-click unsubscribe, and natural-language analytics (Claude → safe read-only SQL over whitelisted views → rows + narrative).

---
---

# Baseline Client (MVP, updated)

The classic mail-client foundation the AI bridge is built on. Retained from v0.1, updated for the daemon architecture.

## Account Management

One or more IMAP accounts; TOML config; passwords via Keychain / password command / env / OAuth2.

```toml
[[accounts]]
name = "Personal"
imap_server = "imap.fastmail.com"
port = 993
username = "user@example.com"
password_command = "security find-generic-password ..."
# smtp for sending
smtp_server = "smtp.fastmail.com"
smtp_port = 587
```

## Synchronization

Background service (in `rmaild`) discovers folders, downloads new mail, updates flags, detects deletions and moves. **IMAP IDLE** pushes changes within seconds; **CONDSTORE/QRESYNC** deltas minimize transfer; falls back to interval polling (default 5 min). `mail sync [--full] [--watch]`. Sync emits events that drive indexing, AI enrichment, rules, and the gRPC event stream.

## Local Storage

SQLite (WAL). Core tables: `accounts`, `mailboxes`, `messages`, `flags`, `attachments`, `threads`, `contacts`, `sync_state` — plus the feature tables (finder, AI, index, tags, notes, outbox, search-feedback, API). Stores full raw RFC822, parsed metadata, thread references, body text, attachment metadata.

## Search

The Part I ranking pipeline. Baseline operators (`from:`, `subject:`, `has:attachment`, `before:`, `after:`, `is:unread`, `is:flagged`, free text) plus semantic, fuzzy, entity, tag, note, and `ai:` operators. FTS5 + sqlite-vec + learned reranking.

## TUI

Folders / Message List / Preview layout, plus the AI panel, Ask pane, Outbox folder, and finder overlay. Navigation `j/k gg G / Enter q ?`; actions archive/delete/mark/copy/move/reply/forward + AI quick-actions. Modal vim keybindings, hot-reloadable.

## CLI

`mail sync | search | list | open | export | stats | accounts` plus every feature verb; global `--format {table,json,ndjson}`; a gRPC client under the hood.

## Message Viewer

Plain text, multipart, quoted-printable, base64, UTF-8, encoded headers. No HTML rendering initially; "Open HTML in browser". Attachments: list / save / open (future preview + Quick Look).

## Offline Mode

Everything except sync, and the cloud-AI paths, works offline. Local AI path keeps summarize/search working with zero egress.

---

# Synchronization & Data-Flow Model

```
IMAP / SMTP
   │
   ▼
Sync Engine (IDLE + QRESYNC)      ── emits events ──┐
   │                                                │
   ▼                                                ▼
SQLite  ◀── Indexing Engine (FTS + vector + entity) │
   │            │                                    │
   │            ▼                                    ▼
   │      AI Enrichment (triage/deep/embed)   Rules / Hooks / Webhooks
   │            │
   ▼            ▼
Retrieval & Ranking Pipeline (Part I)
   │
   ▼
Core Services ──▶ gRPC ──▶ CLI · TUI · MCP (Claude) · gRPC-web · scripts
```

UI components never talk to IMAP directly; they are gRPC clients of `rmaild`.

---

# Master Configuration (merged example)

```toml
# ── accounts ──
[[accounts]]
name = "Personal"
imap_server = "imap.fastmail.com"; port = 993
username = "user@example.com"
password_command = "security find-generic-password -s fastmail -w"
smtp_server = "smtp.fastmail.com"; smtp_port = 587

[sync] interval = "5m"; idle = true; qresync = true

[search] default_mode = "hybrid"; rerank = "auto"; learning = true   # (full block in Part I)
[index] enabled = true; workers = 4                                  # (full block in III-3)
[index.semantic] provider = "local"                                  # privacy default
[ai] enabled = true; provider = "claude"                             # (full block in III-2)
[ai.models] triage = "claude-haiku-4-5"; deep = "claude-opus-4-8"; embedding = "local"
[ai.limits] daily_cost_cap_usd = 5.00; monthly_cost_cap_usd = 100.00
[ai.privacy] redact = true; strip_attachments = true
[tags] default_sync_mode = "auto"                                    # (full block in III-4)
[send] undo_window = "10s"; ai_requires_confirmation = true          # (full block in III-5)
[finder] enabled = true                                              # (full block in III-1)
[grpc] enabled = true; auth = "token"                                # (full block in III-6)

# Per-account AI opt-out / residency
[[accounts]]
name = "Personal-Legal"
[accounts.ai] enabled = false
```

---

# Performance Goals

| Metric | Target |
|---|---|
| Startup (TUI attach to daemon) | < 200 ms |
| First streamed search hit | < 30 ms |
| Full ranked search (no Claude rerank) | < 150 ms |
| Fuzzy finder keystroke → first batch | < 16 ms |
| Open message | < 30 ms |
| New mail → lexically indexed | < 2 s after sync |
| New mail → embedded (semantic) | < 10 s after sync |
| `ask_mailbox` first token | < 1.5 s |
| Memory (daemon) | < 150 MB steady-state |
| Background sync / AI | never freezes UI |

---

# Logging & Error Handling

Structured logging via `tracing` (info/warn/error/debug). The app keeps functioning when the network is unavailable, an account's auth fails, a mailbox is unavailable, or the AI provider is down — the local database and all local features remain fully usable.

---

# Suggested Rust Stack

- **Networking / protocol** — tokio, async-imap, lettre (SMTP), rustls, reqwest (Anthropic API)
- **Mail parsing** — mail-parser, mailparse; html2text (strip)
- **Database** — rusqlite (WAL), refinery (migrations)
- **Search / index** — SQLite FTS5, sqlite-vec (vectors), nucleo (fuzzy), simsearch/strsim, symspell (spellfix), fastembed / ort (ONNX local embeddings + cross-encoder reranker)
- **Ranking** — a GBDT/LTR crate or hand-rolled gradient boosting; ndarray for feature math
- **Extraction** — lopdf/pdfium, calamine (xlsx), zip, tesseract / Apple Vision (OCR)
- **AI** — Anthropic Messages API (reqwest), official Rust MCP SDK
- **gRPC** — tonic, prost, tonic-reflection, tonic-health, tonic-web; buf (proto CI)
- **CLI / TUI** — clap, ratatui, crossterm
- **Serialization / config** — serde, toml
- **Concurrency** — tokio tasks, rayon (parallel ranking/matching)
- **Security** — security-framework (Keychain), argon2, oauth2, ring
- **Logging** — tracing, tracing-subscriber

---

# Milestones

1. **Foundation** — CLI, config, IMAP login, mailbox listing; `rmaild` skeleton.
2. **Storage & sync** — download mail, SQLite schema, incremental + QRESYNC + IDLE sync.
3. **Indexing engine** — FTS5, extraction pipeline, entity index, incremental job queue.
4. **Search & ranking (crown jewel)** — candidate generation, RRF fusion, feature extraction, L1 deterministic ranker, operators, saved searches. *Ship a genuinely great relevance-first search before anything AI-heavy.*
5. **gRPC core API** — service decomposition, streaming, auth/tokens, reflection/health; CLI/TUI become clients.
6. **AI bridge** — Anthropic integration, redaction firewall, triage + deep summaries on new mail, embeddings + semantic/hybrid retrieval, L2 rerank (cross-encoder + Claude), `ask_mailbox` RAG.
7. **MCP server** — auto-projection over gRPC, capability tokens, the full tool set for Claude.
8. **Organize & compose** — tags, notes, smart folders, fuzzy finder; drafts, scheduled send/outbox, AI drafting.
9. **Learning loop & personalization** — feedback logging, offline training, model hot-swap, eval harness.
10. **Automation & insights** — rules/hooks/webhooks, autonomous agent, digests, analytics.
11. **Hardening & packaging** — budgets, audit, injection shield, local-only path; testing, perf, macOS packaging.

---

# Future Features

Gmail label deep integration · HTML rendering · Conversation view polish · JMAP transport · PGP / S/MIME · Multiple profiles · Plugin system · Notification Center / Spotlight / Quick Look integration · Linux port · Cross-device encrypted sync of local index & AI enrichments · Voice query · Learned per-recipient send-time model · On-device fine-tuned reranker.

---

# Success Criteria

The project is successful when:

- **Relevance:** for a benchmarked query set, the intended message is in the **top 3** results ≥ 95% of the time, and the ranking pipeline measurably beats pure-BM25 and pure-semantic baselines on NDCG@10 and MRR — improving further with use via the feedback loop.
- Initial sync completes reliably for mailboxes with **> 100,000 messages**.
- **First search hit renders in < 30 ms** and full ranked results in **< 150 ms** on local data; fuzzy find is instant.
- The UI stays responsive during sync, indexing, and AI enrichment.
- Users can operate entirely offline after synchronization, with local-AI features intact.
- **AI assistants can search, read, organize, draft, schedule, and act on mail exclusively through MCP/gRPC** — no direct IMAP access — under least-privilege capability tokens, redaction, budgets, and an audit trail.
- Every CLI/TUI capability has a corresponding gRPC RPC and (where safe) an MCP tool — verified in CI. No feature drift between the three front doors.

