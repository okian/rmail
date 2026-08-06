//! [`FeatureExtractor`]: prd.md's Stage 3 turned into a fixed, small number
//! of batched SQL round trips plus local Rust computation over task 29's
//! fused candidates.
//!
//! # A fixed query count, not one per candidate or per feature
//!
//! prd.md's performance budget gives Stage 3 well under ten milliseconds for
//! the whole candidate set, and this task's acceptance bullet says so
//! directly: "computed **cheaply**... batch the DB reads rather than issuing
//! per-candidate queries." [`FeatureExtractor::extract_at`] issues **five**
//! queries total, however many candidates it is given: one joined
//! `messages`/`mailboxes`/`threads` row fetch, one `flags` fetch, one
//! `contacts` fetch (keyed by the batch's distinct sender addresses), one
//! `flags`-joined thread-reply-membership fetch, and one field-isolated
//! `bm25()` fetch. Every per-candidate computation after that is pure Rust
//! over already-fetched maps.
//!
//! # Re-parsing `plan.raw`, not `QueryPlan::lexical_terms`
//!
//! Every textual feature here (`bm25_subject`/`bm25_body`/`bm25_from`/
//! `bm25_attach`, `exact_phrase_hit`, `term_coverage`, `proximity_min_span`,
//! `best_match_field`, `has_attachment_match`) is built from
//! `query::parse(&plan.raw)`'s fresh [`ParsedQuery`], not
//! [`QueryPlan::lexical_terms`]/[`QueryPlan::expansions`] — the same choice
//! `retrieve::fanout::Fanout::run_lexical` already makes, and for the same
//! documented reason (see that module's "A known gap: spell-fix and PMI
//! expansion do not reach lexical"): the lexical retriever itself, whose
//! `bm25` score is what `rrf_score`/`SourceHit::score` already carry into
//! this module, was built and ranked against the *raw* parse, not the
//! spell-corrected/expanded plan. Building `bm25_subject` et al. from a
//! *different* term set than what actually produced the candidate's own
//! lexical rank would make this module's numbers describe a query the
//! retriever never ran. This module inherits that known gap rather than
//! fixing it — fixing it is `retrieve::lexical::MatchExpr`/`Fanout`'s job
//! (a follow-up task per `fanout`'s own docs), not this one's.
//!
//! # Two different notions of "textual match," on purpose
//!
//! `bm25_subject`/`bm25_body`/`bm25_from`/`bm25_attach` are computed from a
//! single `AND`-required `MATCH` expression — the same one
//! `retrieve::lexical::MatchExpr` builds for its own ranked query — isolating
//! each field's own weight to `0` for every other call
//! ([`isolated_weights`]) so `bm25(fts_messages, w, 0,0,0,0,0,0)` reports
//! that column's contribution alone. A candidate that only *partially*
//! overlaps the query lexically (found instead by dense/fuzzy/entity, with
//! one matching word and one missing) genuinely fails that `AND`, and
//! legitimately scores `0.0` on every one of these four fields — that
//! mirrors what the real lexical retriever would have decided, which is
//! exactly what a ranking feature named `bm25_*` should mean.
//! [`FeatureVector::term_coverage`], `exact_phrase_hit`, and
//! `proximity_min_span` exist precisely to *not* lose that partial overlap:
//! they are computed by a local, `OR`-per-term scan over the candidate's own
//! fetched text (subject/from/to/cc/body — see [`tokenize_lower`]) rather
//! than another `MATCH`, and credit a candidate for every term it actually
//! contains, whether or not the full `AND` succeeded.
//!
//! # No table yet: `is_pinned`, `has_tag_match`, `ai_priority`,
//! # `prior_opens_from_sender`
//!
//! Four of prd.md's Stage 3 features name a subsystem this build has no
//! table for: pinning, tags (task 55), AI triage priority (task 48/49), and
//! the search-impression/action log (task 64). This mirrors
//! `retrieve::lexical`'s own precedent for the identical situation at query
//! time — `is:pinned`/`tag:`/`ai:` all classify to `RawEffect::Never`
//! there, "provably false today," not "unknown" — and this module makes the
//! same honest call for the same operators read as *features* rather than
//! filters: each is a real, always-reachable computation that currently has
//! no signal to return, not a stub with a branch that never runs. When a
//! later task adds the backing table, only this module's four defaults
//! change; the feature's name, position, and meaning do not, which is the
//! whole point of naming features up front (see `features::name`'s docs).
//!
//! # `sender_affinity` vs. `sender_reputation`
//!
//! Both read the same `contacts` row (message-exchange volume, saturating —
//! see [`SENDER_VOLUME_SATURATE`]) but answer different questions and must
//! not collapse into the same number: `sender_affinity` (personal group) is
//! that volume weighted by *how recently* the user last heard from this
//! sender — a query-time-relevant "how close is this relationship right
//! now." `sender_reputation` (global group) drops the recency term (it is a
//! corpus-wide prior, not a per-query signal) but dampens the same volume by
//! [`REPUTATION_BULK_DAMPING`] when [`FeatureVector::is_newsletter`] or
//! [`FeatureVector::is_automated`] fires — a mailing list the user has
//! "exchanged" thousands of messages with (every issue landing in the inbox)
//! is not thousands of messages of *trust* the way a real correspondent's
//! would be. prd.md's own prose describes `sender_affinity` as "msgs
//! exchanged × reply-ratio × recency"; the reply-ratio factor is
//! deliberately not folded in here — a corpus-wide per-sender reply ratio
//! has no cheap, batched query against today's schema (no index on
//! `messages.from_addr`), and [`FeatureVector::user_replied_thread`] already
//! carries a real, cheap reply signal (this thread specifically, via
//! `flags`/`thread_id`, both already indexed) as its own named feature. A
//! linear or learned ranker (task 31/65) combines the two directly; this
//! module does not need to pre-multiply them into one number to make that
//! possible.
//!
//! # `contacts` is not populated by anything yet
//!
//! `repo::upsert_contact` (task 6) exists but nothing in the ingestion path
//! calls it outside tests today — a pre-existing gap in an earlier task, out
//! of this module's scope to fix. Until it is wired up, [`FeatureExtractor::fetch_contacts`]
//! finds no row for any sender in a real mailbox, and `sender_affinity`/
//! `sender_reputation` degrade to their documented `0.0` default for every
//! candidate. The code is written against the schema as specified, not
//! against today's incomplete wiring, so nothing here needs to change when
//! that gap is closed.
//!
//! # Graceful degradation, and the one place it needs to remember *why*
//! # data is missing
//!
//! Every batched fetch below follows [`crate::fuse::Fuser`]'s contract: a
//! cancelled or failed lookup degrades the feature group it feeds to a
//! documented default rather than failing extraction outright. For four of
//! the five fetches, "no row for this id" and "the whole fetch failed" both
//! collapse to the same safe default (an unknown sender/message/thread
//! contributes `0.0`/`false`, which is true whether the row is genuinely
//! absent or merely unfetched). [`FeatureExtractor::fetch_flags`] is the one
//! exception: a message with **no** `\Seen` flag row is, correctly,
//! `is_unread = true` — but if the *entire* `flags` query failed, collapsing
//! every id to "no rows found" would silently claim every candidate is
//! unread, a false positive claim rather than an honest "we don't know."
//! [`fetch_flags`](FeatureExtractor::fetch_flags) therefore returns
//! `Option<BTreeMap<..>>` — `None` only for a whole-query degrade — so
//! [`FeatureExtractor::build_features`] can tell "fetched, and this message
//! has no flags" apart from "not fetched at all" and default `is_unread`/
//! `is_flagged` to `false` only in the latter case.
//!
//! # Determinism
//!
//! [`FeatureExtractor::extract_at`] takes `now: DateTime<Utc>` as a
//! parameter and never calls [`Utc::now`] itself — every temporal
//! computation (`age_days`, `recency_decay`, `matches_date_intent`, and the
//! recency terms folded into `sender_affinity`/`thread_activity`) is a pure
//! function of `now` plus the local database's own stored timestamps. Given
//! the same `candidates`, `plan`, and `now`, two calls produce a
//! byte-identical [`FeatureVector`] for every candidate — see
//! `extract::tests`'s determinism tests, and [`super::vector`]'s module docs
//! for the serialization side of the same replay contract.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use crate::config::Bm25Weights;
use crate::fuse::FusedCandidate;
use crate::query::{self, DateRange, HardFilter, Mode, ParsedQuery, QueryPlan};
use crate::retrieve::cancel::interruptible_read;
use crate::retrieve::lexical::{has_indexable_content, quote_fts_literal};
use crate::retrieve::Source;
use crate::storage::Database;

use super::vector::{finite, MatchField};
use super::FeatureVector;

/// One candidate's extracted features, paired with the message id they
/// describe. `Serialize`/`Deserialize` for the same reason
/// [`FeatureVector`] itself carries them — task 64 logs this pair verbatim
/// as a `search_impression` row (prd.md's `message_uid` alongside its
/// `features` BLOB), and task 65 replays it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CandidateFeatures {
    /// The matched message (`messages.id`) — same as the input
    /// [`FusedCandidate::message_id`] this vector was built from.
    pub message_id: i64,
    /// The computed vector.
    pub features: FeatureVector,
}

/// Half-life used when configuration supplies a non-positive/non-finite one.
/// Mirrors `retrieve::recency::DEFAULT_HALF_LIFE_DAYS`'s value exactly, kept
/// as its own constant rather than imported (that one is private to
/// `retrieve::recency`) — the same "a three-line duplication beats a
/// cross-module private dependency" call [`super::vector`]'s `source_serde`
/// docs explain for `Source`'s ordinal.
const DEFAULT_HALF_LIFE_DAYS: f64 = 30.0;

/// Seconds in a day, for turning a unix-seconds age into days.
const SECONDS_PER_DAY: f64 = 86_400.0;

/// Largest number of candidates one [`FeatureExtractor::extract_at`] call
/// fetches metadata for. Mirrors `fuse::MAX_META_FETCH`'s exact reasoning: a
/// defensive ceiling on memory/CPU for a pathological config
/// (`search.candidates_per_source` up to `fts::MAX_LIMIT` across seven
/// sources), not a workaround for a real SQLite bound-variable limit.
/// Candidates past the cap still get a [`CandidateFeatures`] entry — the
/// output stays one-to-one with the input — just an all-default one, the
/// same "truncate the tail, never drop a candidate" contract
/// `fuse::Fuser::fetch_meta` uses for its own cap.
const MAX_FEATURE_BATCH: usize = 2_000;

/// Characters of `body_text` fetched per candidate for local text scanning
/// (`term_coverage`, `exact_phrase_hit`, `proximity_min_span`). Mirrors
/// `fuse::MAX_BODY_CHARS_FOR_SIMHASH`'s reasoning: generous relative to what
/// these features need (a query term either appears somewhere in a
/// realistic message or the candidate came from a different source
/// entirely), capped so a handful of very long bodies cannot dominate one
/// batch's memory/CPU.
const MAX_BODY_CHARS_FOR_SCAN: i64 = 4_000;

/// Saturation point for "messages exchanged with this sender" —
/// `sender_affinity`/`sender_reputation`'s volume term. Matches
/// `query::plan::CONTACT_BOOST_SATURATE`'s own value and reasoning (beyond
/// this many exchanged messages, more history does not make the signal any
/// more certain); kept as its own constant since that one is private to
/// `query::plan`.
const SENDER_VOLUME_SATURATE: i64 = 20;

/// Saturation point for "messages in this thread" when scoring
/// `thread_activity` — a thread this size or larger is unambiguously active;
/// more messages past this point say nothing new about *how* active.
const THREAD_SIZE_SATURATE: i64 = 10;

/// How much `sender_reputation` is dampened for a sender heuristically
/// detected as newsletter/automated. High message volume from a mailing
/// list is not the same evidence of trust as high volume from a person, so
/// the raw exchange-count signal is scaled down rather than zeroed — a
/// detected-bulk sender the user still opens and reads is not *zero*
/// reputation, just markedly less than a real correspondent's.
const REPUTATION_BULK_DAMPING: f64 = 0.3;

/// Lowercased substrings whose presence in a from-address/display-name marks
/// a sender as transactional/system mail (prd.md's `is_automated`). A
/// heuristic, not a learned classifier — see the module docs' "No table yet"
/// section for why this one, unlike `ai_priority`, is implemented for real
/// rather than defaulted: it is the only local, cheap signal this build has
/// for "does this look machine-sent," and there is no future table to wait
/// on instead.
const AUTOMATED_SENDER_KEYWORDS: &[&str] = &[
    "noreply",
    "no-reply",
    "no.reply",
    "donotreply",
    "do-not-reply",
    "mailer-daemon",
    "postmaster",
    "notifications",
    "notification",
    "alerts@",
    "alert@",
    "automated",
    "auto-confirm",
    "system@",
];

/// As [`AUTOMATED_SENDER_KEYWORDS`], for prd.md's `is_newsletter` — bulk
/// marketing mail specifically, a narrower category than "automated" (a
/// password-reset email is automated but not a newsletter).
const NEWSLETTER_SENDER_KEYWORDS: &[&str] = &[
    "newsletter",
    "digest",
    "marketing",
    "campaign",
    "bulletin",
    "updates@",
    "news@",
];

/// Batched lookups for one [`FeatureExtractor::extract_at`] call, plus the
/// parsed-query terms every candidate's textual features scan against.
/// Exists purely to keep [`build_features`]'s parameter list to "one
/// candidate, one bundle" instead of eight positional maps.
///
/// `Default` gives the five *fetch-result* fields (`core`, `flags`,
/// `contacts`, `replied_threads`, `bm25`) their documented degrade default —
/// an empty map/set for "no row found," `None` for `flags`'s
/// whole-fetch-degraded case — so [`FeatureExtractor::extract_at`]'s
/// `spawn_blocking` join-failure fallback can reuse it directly for those
/// five. `scan_terms`/`scan_phrases` are different: they are pure
/// derivations of the parsed query, not a fetch result, so an *empty*
/// `Vec` there is not "unknown," it is the specific claim "the query had no
/// free-text terms" — which `term_coverage`'s own vacuous-truth rule reads
/// as maximal coverage, exactly backwards for a degrade path. The
/// join-failure fallback overrides both fields with their real,
/// already-computed values rather than trusting `Default` for them; see
/// that call site.
#[derive(Default)]
struct BatchData {
    core: BTreeMap<i64, CoreRow>,
    /// `None` only when the whole `flags` fetch degraded — see the module
    /// docs' "Graceful degradation" section for why this one field, alone
    /// among the five fetches, needs to distinguish that from "fetched, and
    /// this message has no flags."
    flags: Option<BTreeMap<i64, BTreeSet<String>>>,
    contacts: BTreeMap<String, ContactRow>,
    replied_threads: BTreeSet<i64>,
    bm25: BTreeMap<i64, Bm25Fields>,
    scan_terms: Vec<String>,
    scan_phrases: Vec<String>,
}

/// One message's joined `messages`/`mailboxes`/`threads` row.
#[derive(Debug, Clone)]
struct CoreRow {
    thread_id: Option<i64>,
    subject: Option<String>,
    from_addr: Option<String>,
    from_name: Option<String>,
    to_addrs: Option<String>,
    cc_addrs: Option<String>,
    ts: Option<i64>,
    body_len: i64,
    body_excerpt: Option<String>,
    mailbox_name: String,
    thread_root_message_id: Option<i64>,
    thread_size: i64,
    thread_last_message_at: Option<i64>,
}

/// One sender's `contacts` row.
#[derive(Debug, Clone, Copy, Default)]
struct ContactRow {
    message_count: i64,
    last_seen: Option<i64>,
}

/// One candidate's isolated per-column `bm25()` scores, already sign-flipped
/// to higher-is-better (see [`crate::index::fts`]'s "BM25 signs" note) and
/// [`finite`]-sanitized — every field is safe to compare/store as-is.
#[derive(Debug, Clone, Copy, Default)]
struct Bm25Fields {
    subject: f64,
    from: f64,
    body: f64,
    attach: f64,
}

/// prd.md's Stage 3 feature extractor: turns task 29's fused candidates into
/// [`CandidateFeatures`], reading only what is already indexed locally.
///
/// Cheap to clone: `db` shares a connection pool, and `bm25_weights`/
/// `half_life_days` are small `Copy`-ish values — the same pattern
/// [`crate::index::fts::FtsIndex`]/[`crate::retrieve::LexicalRetriever`] use.
#[derive(Debug, Clone)]
pub struct FeatureExtractor {
    db: Database,
    bm25_weights: Bm25Weights,
    half_life_days: f64,
}

impl FeatureExtractor {
    /// Build an extractor over `db`, field-weighting the `bm25_*` features by
    /// `bm25_weights` (`search.bm25_weights` — the same config the lexical
    /// retriever itself uses, so these features mean the same "field weight"
    /// prd.md's cascade means everywhere else) and decaying recency with
    /// `recency_half_life_days` (`search.retrievers.recency_half_life_days`
    /// — the same half-life the recency-prior retriever itself decays by).
    /// A non-positive/non-finite half-life is clamped to
    /// [`DEFAULT_HALF_LIFE_DAYS`], mirroring
    /// `retrieve::recency::RecencyRetriever::new`'s own validation: an
    /// untrusted config value must not divide-by-zero or flip the sign of
    /// every candidate's decay.
    #[must_use]
    pub fn new(db: Database, bm25_weights: Bm25Weights, recency_half_life_days: f64) -> Self {
        let half_life_days = if recency_half_life_days.is_finite() && recency_half_life_days > 0.0 {
            recency_half_life_days
        } else {
            tracing::warn!(
                configured = recency_half_life_days,
                default = DEFAULT_HALF_LIFE_DAYS,
                "recency half-life must be a positive, finite number of days; using the default"
            );
            DEFAULT_HALF_LIFE_DAYS
        };
        Self {
            db,
            bm25_weights,
            half_life_days,
        }
    }

    /// As [`FeatureExtractor::extract_at`], anchored to the current instant.
    /// Real callers (the live search path) want this; [`extract_at`](Self::extract_at)
    /// is what makes the extraction itself reproducible in a test or a
    /// task-65 replay.
    pub async fn extract(
        &self,
        candidates: &[FusedCandidate],
        plan: &QueryPlan,
        cancel: &CancellationToken,
    ) -> Vec<CandidateFeatures> {
        self.extract_at(candidates, plan, Utc::now(), cancel).await
    }

    /// Extract a [`FeatureVector`] for every candidate in `candidates`, in
    /// the same order and count they were given.
    ///
    /// See the module docs for `now`'s determinism contract and for why this
    /// never fails outright — a degraded lookup only defaults the feature
    /// group it feeds.
    #[tracing::instrument(
        skip(self, candidates, plan, cancel),
        fields(candidates = candidates.len(), extracted)
    )]
    pub async fn extract_at(
        &self,
        candidates: &[FusedCandidate],
        plan: &QueryPlan,
        now: DateTime<Utc>,
        cancel: &CancellationToken,
    ) -> Vec<CandidateFeatures> {
        if candidates.is_empty() {
            return Vec::new();
        }

        let capped: Vec<i64> = if candidates.len() > MAX_FEATURE_BATCH {
            tracing::warn!(
                len = candidates.len(),
                cap = MAX_FEATURE_BATCH,
                "fused candidate set exceeds the feature-extraction batch cap; \
                 tail candidates still returned, with default (unfetched) features"
            );
            candidates[..MAX_FEATURE_BATCH]
                .iter()
                .map(|c| c.message_id)
                .collect()
        } else {
            candidates.iter().map(|c| c.message_id).collect()
        };

        // `parsed`/`required_match` are pure, in-memory (no I/O — see
        // `query::parse`'s own docs), so computing them before the fetches
        // below costs nothing and lets `fetch_bm25_fields` join the other
        // two fetches that do not depend on `core` rather than trail behind
        // them.
        let parsed = query::parse(&plan.raw);
        let required_match = build_required_match(&parsed);

        // `fetch_core`, `fetch_flags`, and `fetch_bm25_fields` read
        // different tables and share no data dependency — the same
        // `tokio::join!`-is-the-bounded-pool reasoning
        // `retrieve::fanout::Fanout::generate` gives its own concurrent
        // fetches applies here: each `.await` is a `spawn_blocking` join
        // against `storage::Database`'s own pooled read connections, so
        // running the three concurrently is bounded by that pool already,
        // not by anything this function would need to add.
        let (core, flags, bm25) = tokio::join!(
            self.fetch_core(&capped, cancel),
            self.fetch_flags(&capped, cancel),
            async {
                match &required_match {
                    Some(expr) => self.fetch_bm25_fields(expr, &capped, cancel).await,
                    None => BTreeMap::new(),
                }
            },
        );

        // `fetch_contacts`/`fetch_replied_threads` both need `core`'s
        // distinct addresses/thread ids first, but not each other's output,
        // so they still run concurrently with one another.
        let from_addrs: BTreeSet<String> = core
            .values()
            .filter_map(|row| row.from_addr.as_deref())
            .map(str::to_lowercase)
            .collect();
        let thread_ids: BTreeSet<i64> = core.values().filter_map(|row| row.thread_id).collect();
        let (contacts, replied_threads) = tokio::join!(
            self.fetch_contacts(&from_addrs, cancel),
            self.fetch_replied_threads(&thread_ids, cancel),
        );

        // `scan_terms`/`scan_phrases` are pure derivations of `parsed`, not
        // fetch results — unlike every other `BatchData` field, their empty
        // value is *not* a safe "degraded" default (an empty `scan_terms`
        // reads as `term_coverage`'s vacuous-truth `1.0`, the wrong answer
        // for "we don't know," not "the query had nothing to cover"), so
        // both are computed once, here, and reused verbatim on the
        // `spawn_blocking` join-failure fallback below rather than left to
        // `BatchData::default()`.
        let scan_terms = scan_terms(&parsed);
        let scan_phrases = scan_phrases(&parsed);
        let batch = BatchData {
            core,
            flags,
            contacts,
            replied_threads,
            bm25,
            scan_terms: scan_terms.clone(),
            scan_phrases: scan_phrases.clone(),
        };

        // The only part of `plan` `build_features` actually reads is its
        // resolved date scope (`matches_date_intent`) — precomputed once
        // here, rather than re-walking `plan.hard_filters` inside the
        // per-candidate loop below (a repeated `O(candidates)` allocation
        // for an `O(1)` result), and passed as its own small `Vec` instead
        // of cloning the whole `QueryPlan` (which carries `query_vector`, a
        // full embedding, into a closure that never reads it).
        let date_ranges = date_scope_ranges(plan);

        // Building every candidate's vector is pure CPU (tokenizing up to
        // [`MAX_FEATURE_BATCH`] candidates' worth of subject/body text — see
        // [`build_features`]) with no `.await` of its own, so left inline it
        // would hold whatever tokio worker thread is polling this future for
        // however long that scan takes — exactly the "never block the
        // runtime" rule `CLAUDE.md`'s non-negotiables state and
        // `fuse::Fuser::fuse` already routes its own analogous per-candidate
        // CPU pass (SimHash fingerprinting) around via `spawn_blocking`. The
        // owned clones below are `spawn_blocking`'s own `'static` requirement
        // — `batch` was already about to be dropped at the end of this
        // function, so moving it costs nothing; `candidates`/`date_ranges`
        // are still needed by the join-failure fallback path afterward, so
        // that closure gets its own clones rather than consuming the
        // originals.
        let candidates_owned = candidates.to_vec();
        let date_ranges_owned = date_ranges.clone();
        let half_life_days = self.half_life_days;
        let out = match tokio::task::spawn_blocking(move || {
            candidates_owned
                .iter()
                .map(|candidate| CandidateFeatures {
                    message_id: candidate.message_id,
                    features: build_features(
                        candidate,
                        &batch,
                        &date_ranges_owned,
                        now,
                        half_life_days,
                    ),
                })
                .collect::<Vec<CandidateFeatures>>()
        })
        .await
        {
            Ok(out) => out,
            Err(join_error) => {
                // Should not happen — `build_features` has no panics — but a
                // join failure must degrade like every other failure mode in
                // this module, not lose the whole extraction. An all-default
                // vector per candidate is the same "unknown, not wrong"
                // answer every other degraded fetch in this module gives —
                // *except* `scan_terms`/`scan_phrases`, which are not fetch
                // results and must keep their real (already-computed)
                // values rather than `BatchData::default()`'s empty ones:
                // an empty `scan_terms` would make `term_coverage` read as
                // its vacuous-truth `1.0` ("nothing to cover"), the wrong
                // answer for "we don't know," not the right one for "the
                // query had no free-text terms."
                tracing::warn!(%join_error, "feature-vector build task failed; returning defaults");
                let empty = BatchData {
                    scan_terms,
                    scan_phrases,
                    ..BatchData::default()
                };
                candidates
                    .iter()
                    .map(|candidate| CandidateFeatures {
                        message_id: candidate.message_id,
                        // `build_features` over an empty batch still reads
                        // real data straight off `candidate` itself
                        // (`rrf_score`/`num_sources_hit`/`best_source`/
                        // `cos_*`/`fuzzy_score`) — only the DB-derived
                        // fields degrade to their documented defaults, the
                        // same partial-degrade every other fetch failure in
                        // this module already produces.
                        features: build_features(
                            candidate,
                            &empty,
                            &date_ranges,
                            now,
                            half_life_days,
                        ),
                    })
                    .collect()
            }
        };

        tracing::Span::current().record("extracted", out.len());
        out
    }
    /// One joined `messages`/`mailboxes`/`threads` row per id, in a single
    /// round trip. Missing ids (a candidate whose message was deleted
    /// between retrieval and feature extraction, or a lookup that degraded
    /// entirely) are simply absent from the returned map — every reader in
    /// [`FeatureExtractor::build_features`] treats "no row" as "unknown
    /// message," the same default whether one id or every id is missing.
    async fn fetch_core(&self, ids: &[i64], cancel: &CancellationToken) -> BTreeMap<i64, CoreRow> {
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let placeholders = placeholder_list(ids.len());
        let sql = format!(
            "SELECT m.id, m.thread_id, m.subject, m.from_addr, m.from_name, m.to_addrs, \
                    m.cc_addrs, COALESCE(m.date, m.internaldate), \
                    LENGTH(COALESCE(m.body_text, '')), \
                    SUBSTR(COALESCE(m.body_text, ''), 1, {MAX_BODY_CHARS_FOR_SCAN}), \
                    mb.name, t.root_message_id, COALESCE(t.message_count, 0), t.last_message_at \
             FROM messages m \
             JOIN mailboxes mb ON mb.id = m.mailbox_id \
             LEFT JOIN threads t ON t.id = m.thread_id \
             WHERE m.id IN ({placeholders})"
        );
        let ids_owned = ids.to_vec();
        let result = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = ids_owned
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    CoreRow {
                        thread_id: row.get(1)?,
                        subject: row.get(2)?,
                        from_addr: row.get(3)?,
                        from_name: row.get(4)?,
                        to_addrs: row.get(5)?,
                        cc_addrs: row.get(6)?,
                        ts: row.get(7)?,
                        body_len: row.get(8)?,
                        body_excerpt: row.get(9)?,
                        mailbox_name: row.get(10)?,
                        thread_root_message_id: row.get(11)?,
                        thread_size: row.get(12)?,
                        thread_last_message_at: row.get(13)?,
                    },
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;
        match result {
            Ok(Some(rows)) => rows.into_iter().collect(),
            Ok(None) => {
                tracing::debug!("core feature lookup cancelled; degrading to defaults");
                BTreeMap::new()
            }
            Err(error) => {
                tracing::warn!(%error, "core feature lookup failed; degrading to defaults");
                BTreeMap::new()
            }
        }
    }

    /// Every IMAP flag on every candidate, one round trip. `None` only on a
    /// whole-fetch degrade — see the module docs' "Graceful degradation"
    /// section for why `is_unread`/`is_flagged` cannot simply treat that the
    /// same as "no flags."
    async fn fetch_flags(
        &self,
        ids: &[i64],
        cancel: &CancellationToken,
    ) -> Option<BTreeMap<i64, BTreeSet<String>>> {
        if ids.is_empty() {
            return Some(BTreeMap::new());
        }
        let placeholders = placeholder_list(ids.len());
        let sql =
            format!("SELECT message_id, flag FROM flags WHERE message_id IN ({placeholders})");
        let ids_owned = ids.to_vec();
        let result = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = ids_owned
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;
        match result {
            Ok(Some(rows)) => {
                let mut out: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
                for (id, flag) in rows {
                    out.entry(id).or_default().insert(flag);
                }
                Some(out)
            }
            Ok(None) => {
                tracing::debug!("flag lookup cancelled; is_unread/is_flagged default to false");
                None
            }
            Err(error) => {
                tracing::warn!(%error, "flag lookup failed; is_unread/is_flagged default to false");
                None
            }
        }
    }

    /// `contacts` rows for `addrs` (already lowercased), one round trip.
    async fn fetch_contacts(
        &self,
        addrs: &BTreeSet<String>,
        cancel: &CancellationToken,
    ) -> BTreeMap<String, ContactRow> {
        if addrs.is_empty() {
            return BTreeMap::new();
        }
        let placeholders = placeholder_list(addrs.len());
        // `lower(address)` on both sides: `contacts.address` is not
        // guaranteed already-lowercase by the schema (`query::plan::match_contacts`
        // makes the identical observation about this same column), and
        // `lower()` is SQLite's ASCII-only fold — the same one every other
        // address comparison in this codebase relies on, since an email
        // address is effectively always ASCII (IDNA-encoded domains).
        let sql =
            format!("SELECT lower(address), message_count, last_seen FROM contacts WHERE lower(address) IN ({placeholders})");
        let addrs_owned: Vec<String> = addrs.iter().cloned().collect();
        let result = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = addrs_owned
                .iter()
                .map(|a| a as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ContactRow {
                        message_count: row.get(1)?,
                        last_seen: row.get(2)?,
                    },
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;
        match result {
            Ok(Some(rows)) => rows.into_iter().collect(),
            Ok(None) => {
                tracing::debug!(
                    "contact lookup cancelled; sender_affinity/reputation default to 0"
                );
                BTreeMap::new()
            }
            Err(error) => {
                tracing::warn!(%error, "contact lookup failed; sender_affinity/reputation default to 0");
                BTreeMap::new()
            }
        }
    }

    /// Which of `thread_ids` contain at least one `\Answered` message, one
    /// round trip.
    async fn fetch_replied_threads(
        &self,
        thread_ids: &BTreeSet<i64>,
        cancel: &CancellationToken,
    ) -> BTreeSet<i64> {
        if thread_ids.is_empty() {
            return BTreeSet::new();
        }
        let placeholders = placeholder_list(thread_ids.len());
        let sql = format!(
            "SELECT DISTINCT m.thread_id FROM messages m \
             JOIN flags f ON f.message_id = m.id AND f.flag = '\\Answered' \
             WHERE m.thread_id IN ({placeholders})"
        );
        let ids_owned: Vec<i64> = thread_ids.iter().copied().collect();
        let result = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = ids_owned
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |row| row.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;
        match result {
            Ok(Some(rows)) => rows.into_iter().collect(),
            Ok(None) => {
                tracing::debug!(
                    "thread-reply lookup cancelled; user_replied_thread defaults to false"
                );
                BTreeSet::new()
            }
            Err(error) => {
                tracing::warn!(%error, "thread-reply lookup failed; user_replied_thread defaults to false");
                BTreeSet::new()
            }
        }
    }

    /// Isolated per-column `bm25()` scores for every id that satisfies
    /// `match_expr`, one round trip — see the module docs' "Two different
    /// notions of textual match" section for why this is `AND`-required
    /// rather than an OR-per-term scan.
    async fn fetch_bm25_fields(
        &self,
        match_expr: &str,
        ids: &[i64],
        cancel: &CancellationToken,
    ) -> BTreeMap<i64, Bm25Fields> {
        if ids.is_empty() {
            return BTreeMap::new();
        }
        // Column order matches `index::fts::COLUMNS`: subject=0, sender=1,
        // recipients=2, body=3, attachments=4, notes=5, summary=6.
        let subject_w = isolated_weights(sane_weight(self.bm25_weights.subject), 0);
        let from_w = isolated_weights(sane_weight(self.bm25_weights.from), 1);
        let body_w = isolated_weights(sane_weight(self.bm25_weights.body), 3);
        let attach_w = isolated_weights(sane_weight(self.bm25_weights.attachments), 4);
        let placeholders = placeholder_list(ids.len());
        let sql = format!(
            "SELECT rowid, bm25(fts_messages, {subject_w}), bm25(fts_messages, {from_w}), \
                    bm25(fts_messages, {body_w}), bm25(fts_messages, {attach_w}) \
             FROM fts_messages WHERE fts_messages MATCH ? AND rowid IN ({placeholders})"
        );
        let match_owned = match_expr.to_owned();
        let ids_owned = ids.to_vec();
        let result = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids_owned.len() + 1);
            params.push(&match_owned);
            for id in &ids_owned {
                params.push(id);
            }
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    sanitize_bm25_row(
                        row.get::<_, f64>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, f64>(3)?,
                        row.get::<_, f64>(4)?,
                    ),
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;
        match result {
            Ok(Some(rows)) => rows.into_iter().collect(),
            Ok(None) => {
                tracing::debug!("bm25 field lookup cancelled; bm25_* default to 0");
                BTreeMap::new()
            }
            Err(error) => {
                // A malformed `MATCH` expression is the only realistic cause
                // (quoting defends against injection, not against every FTS5
                // syntax edge case) — logged and degraded rather than failing
                // extraction, matching every other fetch in this module.
                tracing::warn!(%error, "bm25 field lookup failed; bm25_* default to 0");
                BTreeMap::new()
            }
        }
    }
}

/// Compute one candidate's [`FeatureVector`] from already-fetched batch
/// data — no I/O, pure over its inputs (which is what makes the
/// determinism/serialization tests able to exercise this indirectly through
/// [`FeatureExtractor::extract_at`] without needing a database fixture per
/// case, and what makes it safe to run on the blocking pool inside a
/// `move` closure rather than as a method borrowing `&self`).
fn build_features(
    candidate: &FusedCandidate,
    batch: &BatchData,
    date_ranges: &[(DateRange, bool)],
    now: DateTime<Utc>,
    half_life_days: f64,
) -> FeatureVector {
    let core = batch.core.get(&candidate.message_id);
    // Whether this candidate satisfied the strict `AND`-required `MATCH`
    // (see `build_required_match`) is itself meaningful, not just the
    // isolated scores it produced — [`term_coverage`]'s own computation
    // below reads it directly, not merely `bm25`'s values.
    let bm25_matched = batch.bm25.contains_key(&candidate.message_id);
    let bm25 = batch
        .bm25
        .get(&candidate.message_id)
        .copied()
        .unwrap_or_default();

    let dense_hit = candidate.hits.iter().find(|h| h.source == Source::Dense);
    let fuzzy_hit = candidate.hits.iter().find(|h| h.source == Source::Fuzzy);

    let from_addr_lower = core
        .and_then(|c| c.from_addr.as_deref())
        .map(str::to_lowercase);
    let from_name_lower = core
        .and_then(|c| c.from_name.as_deref())
        .map(str::to_lowercase);
    let (is_newsletter, is_automated) =
        detect_bulk_sender(from_addr_lower.as_deref(), from_name_lower.as_deref());

    let contact = from_addr_lower
        .as_deref()
        .and_then(|a| batch.contacts.get(a));

    let ts = core.and_then(|c| c.ts);
    let age = age_days(ts, now);
    let recency = recency_decay(ts, now, half_life_days);

    // `bm25_matched` is authoritative and takes priority over the local
    // scan below: it comes from FTS5 evaluating the exact same
    // required-terms `AND` over the message's *full*, untruncated text,
    // while the local scan only sees [`MAX_BODY_CHARS_FOR_SCAN`] of body
    // — cheap for its own sake, but a message longer than the cap with
    // every term present past it would otherwise report `bm25_subject`/
    // `bm25_body` positive while `term_coverage` claimed the terms were
    // absent, a contradiction two features in the same vector must never
    // hand a consumer. When the match failed (or nothing lexically
    // eligible was in the query at all), the local scan is still the
    // right fallback: it credits *partial* overlap a strict `AND` cannot
    // (see the module docs' "Two different notions of textual match").
    let haystack_tokens: BTreeSet<String> = core
        .map(|c| tokenize_lower(&joined_text(c)))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let term_coverage_value = if bm25_matched && !batch.scan_terms.is_empty() {
        1.0
    } else {
        term_coverage(&batch.scan_terms, &haystack_tokens)
    };

    let phrase_haystack = core
        .map(|c| normalize_ws(&subject_and_body(c).to_lowercase()))
        .unwrap_or_default();
    let phrase_hit = exact_phrase_hit(&batch.scan_phrases, &phrase_haystack);

    let ordered_tokens: Vec<String> = core
        .map(|c| tokenize_lower(&subject_and_body(c)))
        .unwrap_or_default();
    let span = proximity_min_span(&batch.scan_terms, &ordered_tokens);

    let best_field = best_match_field(&bm25);
    let has_attachment_match = bm25.attach > 0.0;

    let is_thread_root = core.is_some_and(|c| {
        c.thread_id.is_some() && c.thread_root_message_id == Some(candidate.message_id)
    });
    let thread_size = core
        .map(|c| u32::try_from(c.thread_size.max(0)).unwrap_or(u32::MAX))
        .unwrap_or(0);
    let thread_activity = core
        .map(|c| {
            saturate(c.thread_size, THREAD_SIZE_SATURATE)
                * recency_decay(c.thread_last_message_at, now, half_life_days)
        })
        .unwrap_or(0.0);

    let user_replied_thread = core
        .and_then(|c| c.thread_id)
        .is_some_and(|t| batch.replied_threads.contains(&t));

    let sender_affinity = contact
        .map(|c| {
            saturate(c.message_count, SENDER_VOLUME_SATURATE)
                * recency_decay(c.last_seen, now, half_life_days)
        })
        .unwrap_or(0.0);
    let sender_reputation = contact
        .map(|c| {
            let volume = saturate(c.message_count, SENDER_VOLUME_SATURATE);
            if is_newsletter || is_automated {
                volume * REPUTATION_BULK_DAMPING
            } else {
                volume
            }
        })
        .unwrap_or(0.0);

    let (is_unread, is_flagged) = match &batch.flags {
        Some(map) => {
            let set = map.get(&candidate.message_id);
            let seen = set.is_some_and(|f| f.contains("\\Seen"));
            let flagged = set.is_some_and(|f| f.contains("\\Flagged"));
            (!seen, flagged)
        }
        // The whole fetch degraded — see the module docs' "Graceful
        // degradation" section for why this defaults to "no claim"
        // rather than reusing the "no row" branch above.
        None => (false, false),
    };

    FeatureVector {
        bm25_subject: finite(bm25.subject),
        bm25_body: finite(bm25.body),
        bm25_from: finite(bm25.from),
        bm25_attach: finite(bm25.attach),
        exact_phrase_hit: phrase_hit,
        term_coverage: finite(term_coverage_value),
        proximity_min_span: span,
        best_match_field: best_field,
        fuzzy_score: finite(fuzzy_hit.map_or(0.0, |h| h.score)),
        cos_max_chunk: finite(dense_hit.map_or(0.0, |h| h.score)),
        cos_mean_chunk: finite(dense_hit.and_then(|h| h.mean_score).unwrap_or(0.0)),
        rrf_score: finite(candidate.fused_score),
        num_sources_hit: u32::try_from(candidate.num_sources_hit).unwrap_or(u32::MAX),
        best_source: candidate.best_source,
        sender_affinity: finite(sender_affinity),
        user_replied_thread,
        prior_opens_from_sender: 0.0,
        thread_activity: finite(thread_activity),
        age_days: age.map(finite),
        recency_decay: finite(recency),
        matches_date_intent: matches_date_intent(ts, date_ranges),
        is_unread,
        is_flagged,
        is_pinned: false,
        ai_priority: 0.0,
        has_tag_match: false,
        folder_prior: finite(core.map_or(0.0, |c| folder_prior(&c.mailbox_name))),
        has_attachment_match,
        is_thread_root,
        thread_size,
        msg_length: u32::try_from(core.map_or(0, |c| c.body_len.max(0))).unwrap_or(u32::MAX),
        sender_reputation: finite(sender_reputation),
        is_newsletter,
        is_automated,
    }
}

/// `?, ?, ..., ?` for `n` placeholders — every batched `IN (...)` query in
/// this module uses plain, unnumbered `?`s (rather than fuse.rs's `?1, ?2,
/// ...`) because [`FeatureExtractor::fetch_bm25_fields`] has an earlier `?`
/// in the same statement (the `MATCH` argument) that must bind first; mixing
/// numbered and unnumbered placeholders in one statement is exactly the
/// footgun `retrieve::lexical::search_ranked`'s own binding-order note warns
/// about, so every fetch here uses the same convention for consistency
/// rather than switching per query.
fn placeholder_list(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// A weight that cannot invert or poison a `bm25()` ordering — mirrors
/// `index::fts::sane`'s exact check (that one is private to `index::fts`).
fn sane_weight(weight: f64) -> f64 {
    if weight.is_finite() && weight >= 0.0 {
        weight
    } else {
        0.0
    }
}

/// A `bm25()` weight-list argument with only `column_index` non-zero — the
/// isolation trick the module docs describe: every other column's weight is
/// `0`, so the call reports that one column's contribution alone.
fn isolated_weights(weight: f64, column_index: usize) -> String {
    let mut columns = [0.0_f64; 7];
    columns[column_index] = weight;
    columns
        .iter()
        .map(f64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build a [`Bm25Fields`] from the four raw `bm25()` column values (still
/// negative-is-better, per [`crate::index::fts`]'s "BM25 signs" note) —
/// flips the sign and [`finite`]-sanitizes each one here, in a small, pure,
/// directly-testable function, rather than inline inside
/// [`FeatureExtractor::fetch_bm25_fields`]'s `query_map` closure. Both
/// [`best_match_field`] and `has_attachment_match` (in [`build_features`])
/// compare a [`Bm25Fields`] field with `> 0.0`, so a stray non-finite value
/// reaching either undetected would desync them from the sanitized
/// `bm25_*` features [`FeatureVector`] actually reports — this is the one
/// place that can never happen, and `extract::tests` exercises it directly
/// with values SQLite's own `bm25()` would never actually produce (`NaN`,
/// `±inf`), rather than only ever seeing whatever finite value a real
/// index happens to compute.
fn sanitize_bm25_row(subject: f64, from: f64, body: f64, attach: f64) -> Bm25Fields {
    Bm25Fields {
        subject: finite(-subject),
        from: finite(-from),
        body: finite(-body),
        attach: finite(-attach),
    }
}

/// The required-terms `MATCH` expression `retrieve::lexical::MatchExpr`
/// would build for `parsed` — the `full` field only, with no proximity
/// probe (this module has no `NEAR()` bonus to compute). A narrow,
/// deliberate duplication of that struct's assembly loop, not its
/// injection-sensitive quoting (which is reused directly via
/// [`quote_fts_literal`]) — see [`super`]'s module docs for why a private
/// struct from another task's module is not reached into directly.
///
/// Returns `None` under the identical conditions `MatchExpr::build` does:
/// no free-text terms/phrases eligible for lexical matching at all.
fn build_required_match(parsed: &ParsedQuery) -> Option<String> {
    let mut required = Vec::new();
    let mut excluded = Vec::new();
    for term in &parsed.terms {
        if term.mode == Mode::Semantic || !has_indexable_content(&term.text) {
            continue;
        }
        let literal = quote_fts_literal(&term.text);
        if term.negated {
            excluded.push(literal);
        } else {
            required.push(literal);
        }
    }
    for phrase in &parsed.phrases {
        if phrase.mode == Mode::Semantic || !has_indexable_content(&phrase.text) {
            continue;
        }
        let literal = quote_fts_literal(&phrase.text);
        if phrase.negated {
            excluded.push(literal);
        } else {
            required.push(literal);
        }
    }
    if required.is_empty() {
        return None;
    }
    let positive = required.join(" AND ");
    Some(if excluded.is_empty() {
        positive
    } else {
        format!("({positive}) NOT ({})", excluded.join(" OR "))
    })
}

/// Non-negated, non-`~`-forced-semantic free-text terms, lowercased, for the
/// local `term_coverage`/`proximity_min_span` scan.
///
/// The same eligibility filter as [`build_required_match`]'s `required` list
/// (`~`-forced-semantic excluded too), and not by coincidence:
/// [`FeatureExtractor::build_features`]'s `term_coverage` short-circuit to
/// `1.0` when `bm25_matched` is true is only sound if "the strict `AND`
/// succeeded" and "every term this function returns is present" describe
/// the identical term set — a `scan_terms` that included a `~word`
/// `build_required_match` had already excluded would let that shortcut claim
/// full coverage for a term the `AND` never actually checked. A `~`-forced
/// term is excluded from *both* for the same underlying reason either way:
/// the user asked to bypass exact lexical matching for it (`query::parse::Mode`'s
/// doc comment), so its literal textual presence is not evidence the term
/// coverage/proximity features exist to report.
///
/// Deduplicated (first occurrence kept): `"invoice invoice"` must not make
/// [`term_coverage`]'s denominator `2` for one distinct word, which would
/// silently disagree with [`proximity_min_span`]'s own distinct-term
/// handling on the exact same input.
fn scan_terms(parsed: &ParsedQuery) -> Vec<String> {
    let mut seen = BTreeSet::new();
    parsed
        .terms
        .iter()
        .filter(|t| !t.negated && t.mode != Mode::Semantic && has_indexable_content(&t.text))
        .map(|t| t.text.to_lowercase())
        .filter(|text| seen.insert(text.clone()))
        .collect()
}

/// Non-negated phrases, lowercased and whitespace-normalized, for the local
/// `exact_phrase_hit` scan.
fn scan_phrases(parsed: &ParsedQuery) -> Vec<String> {
    parsed
        .phrases
        .iter()
        .filter(|p| !p.negated && has_indexable_content(&p.text))
        .map(|p| normalize_ws(&p.text.to_lowercase()))
        .collect()
}

/// Every run of non-alphanumeric characters becomes one boundary; the
/// result is lowercased whole tokens — the same coarse tokenization
/// `term_coverage`/`proximity_min_span` need and nothing more precise
/// (this is a cheap local scan, not a second index).
fn tokenize_lower(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Collapse every run of whitespace to a single space — makes a phrase
/// substring check robust to a body's line-wrapping without doing real
/// tokenization (an exact-phrase check needs adjacency, not just token
/// membership).
fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every field `term_coverage` scans: subject, from-address, from-name,
/// to/cc addresses, and the (capped) body — joined with a separator so a
/// term never falsely spans two fields.
fn joined_text(core: &CoreRow) -> String {
    [
        core.subject.as_deref(),
        core.from_addr.as_deref(),
        core.from_name.as_deref(),
        core.to_addrs.as_deref(),
        core.cc_addrs.as_deref(),
        core.body_excerpt.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
}

/// Subject and body only — the two fields `exact_phrase_hit`/
/// `proximity_min_span` scan. Narrower than [`joined_text`] on purpose: a
/// phrase or a proximity window spanning into a `from:`/`to:` address field
/// is not the "reads like a sentence" match either feature is meant to
/// credit.
fn subject_and_body(core: &CoreRow) -> String {
    [core.subject.as_deref(), core.body_excerpt.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fraction of `terms` present as a whole token in `haystack_tokens`.
/// Vacuously `1.0` when `terms` is empty — the query asked to cover zero
/// terms, which every candidate satisfies (the same "no constraint" reading
/// `retrieve::lexical::FilterMask::Unconstrained` gives an absent filter).
fn term_coverage(terms: &[String], haystack_tokens: &BTreeSet<String>) -> f64 {
    if terms.is_empty() {
        return 1.0;
    }
    let hit = terms
        .iter()
        .filter(|t| haystack_tokens.contains(t.as_str()))
        .count();
    hit as f64 / terms.len() as f64
}

/// Whether any of `phrases` appears verbatim in `haystack` (both already
/// lowercased/whitespace-normalized by the caller). `false` when there are
/// no phrases to check — a query with no quoted phrase has nothing "exact"
/// to have hit.
fn exact_phrase_hit(phrases: &[String], haystack: &str) -> bool {
    !phrases.is_empty() && phrases.iter().any(|p| haystack.contains(p.as_str()))
}

/// The token width of the tightest window covering every one of `terms`'
/// distinct values in `ordered_tokens`, or `None` when fewer than two
/// distinct terms are given, or at least one of them never occurs — see
/// [`FeatureVector::proximity_min_span`]'s doc comment for why `None`, not
/// `0`, represents "not applicable."
///
/// Standard "smallest range covering one element from each of k sorted
/// lists": each distinct term's occurrence positions are already sorted
/// (ascending token index); one pointer per term tracks the current
/// candidate window `[min, max]` across all k pointers, and the pointer
/// sitting on the smallest value always advances next (moving any other
/// pointer could only widen the window, never shrink a covering one). `k` is
/// always small (a handful of query terms) and total occurrences bounded by
/// [`MAX_BODY_CHARS_FOR_SCAN`], so the `O(occurrences * k)` cost here is
/// negligible.
fn proximity_min_span(terms: &[String], ordered_tokens: &[String]) -> Option<u32> {
    let mut distinct: Vec<&str> = Vec::new();
    for term in terms {
        if !distinct.contains(&term.as_str()) {
            distinct.push(term.as_str());
        }
    }
    if distinct.len() < 2 {
        return None;
    }

    let mut positions: Vec<Vec<usize>> = Vec::with_capacity(distinct.len());
    for term in &distinct {
        let occurrences: Vec<usize> = ordered_tokens
            .iter()
            .enumerate()
            .filter(|(_, tok)| tok.as_str() == *term)
            .map(|(i, _)| i)
            .collect();
        if occurrences.is_empty() {
            // A required term is entirely absent — no window can cover
            // "every" term.
            return None;
        }
        positions.push(occurrences);
    }

    let k = positions.len();
    let mut ptrs = vec![0usize; k];
    let mut best: Option<u32> = None;
    loop {
        let mut min_idx = 0;
        let mut min_val = positions[0][ptrs[0]];
        let mut max_val = min_val;
        for (i, pos) in positions.iter().enumerate() {
            let v = pos[ptrs[i]];
            if v < min_val {
                min_val = v;
                min_idx = i;
            }
            if v > max_val {
                max_val = v;
            }
        }
        let span = u32::try_from(max_val - min_val + 1).unwrap_or(u32::MAX);
        best = Some(best.map_or(span, |b: u32| b.min(span)));
        ptrs[min_idx] += 1;
        if ptrs[min_idx] >= positions[min_idx].len() {
            break;
        }
    }
    best
}

/// Which field carried the strongest positive isolated `bm25()` signal.
/// Ties (including "every column is `0.0`") resolve toward
/// [`MatchField::None`]/the earliest-declared field in
/// subject/from/body/attachment order — a strict `>` comparison against a
/// running best, checked in that fixed order, so the result is a pure
/// function of `fields` with no dependence on iteration/hashing order.
fn best_match_field(fields: &Bm25Fields) -> MatchField {
    let candidates = [
        (MatchField::Subject, fields.subject),
        (MatchField::From, fields.from),
        (MatchField::Body, fields.body),
        (MatchField::Attachment, fields.attach),
    ];
    let mut best = (MatchField::None, 0.0_f64);
    for (field, value) in candidates {
        if value > best.1 {
            best = (field, value);
        }
    }
    best.0
}

/// Message age in days from `now`, or `None` when the message has neither
/// `date` nor `internaldate`. Clamped at zero — a future-dated message
/// (clock skew, a scheduled send) must not read as having *negative* age.
fn age_days(ts: Option<i64>, now: DateTime<Utc>) -> Option<f64> {
    let ts = ts?;
    Some(((now.timestamp() - ts) as f64 / SECONDS_PER_DAY).max(0.0))
}

/// `exp(-age_days / half_life_days)` — prd.md's exact `recency_decay`
/// formula, identical to `retrieve::recency::RecencyRetriever`'s own scoring
/// (that module's docs note this feature is "the same shape... this
/// retriever computes the feature's raw ingredient" for). `0.0` when age is
/// unknown, rather than `1.0` — an unscored message must not read as
/// maximally recent.
fn recency_decay(ts: Option<i64>, now: DateTime<Utc>, half_life_days: f64) -> f64 {
    match age_days(ts, now) {
        Some(age) => (-age / half_life_days).exp(),
        None => 0.0,
    }
}

/// Every `before:`/`after:`/`on:`/`date:` range the query expressed, paired
/// with its negation — [`matches_date_intent`]'s input, computed **once**
/// per [`FeatureExtractor::extract_at`] call rather than re-walked from
/// `plan.hard_filters` inside the per-candidate loop (the result is
/// identical for every candidate in the batch, so recomputing it up to
/// [`MAX_FEATURE_BATCH`] times is a repeated `O(1)`-result allocation for no
/// reason). `DateRange` is `Copy`, so this is a cheap, small, `'static`-
/// friendly `Vec` — unlike cloning the whole [`QueryPlan`] (which also
/// carries `query_vector`, a full embedding) just to hand
/// `build_features` something it never otherwise reads.
fn date_scope_ranges(plan: &QueryPlan) -> Vec<(DateRange, bool)> {
    plan.hard_filters
        .iter()
        .filter_map(|f| match f {
            HardFilter::Date { filter, range } => Some((*range, filter.negated)),
            HardFilter::Other(_) => None,
        })
        .collect()
}

/// The message's date falls inside every range in `date_ranges` (see
/// [`date_scope_ranges`]). `false` when `date_ranges` is empty — an absent
/// date intent is not positive evidence, unlike an absent hard filter's "no
/// constraint" reading elsewhere in this codebase (see [`term_coverage`]'s
/// doc comment for that contrasting case): here, every retriever this build
/// has already gates hard filters as `WHERE` predicates (see `retrieve`'s
/// module docs), so a candidate that reached this stage with a date filter
/// active has, by construction, already passed it — this feature is
/// forward-looking, for a future *soft* date preference (task 58's NL
/// compile) that is not also a hard gate.
///
/// Each range's `bool` is its filter's negation, and it matters here exactly
/// as it does for every hard filter (`retrieve::filtermask::compile`'s own
/// `HardFilter::Date { filter, range } => (date_effect(range),
/// filter.negated)` is the identical read): `-before:2025-01-01` means "date
/// scope is *outside* this range," not "inside it," and dropping it (as an
/// earlier version of this function did) silently inverted the feature for
/// every negated date operator.
fn matches_date_intent(ts: Option<i64>, date_ranges: &[(DateRange, bool)]) -> bool {
    if date_ranges.is_empty() {
        return false;
    }
    let Some(ts) = ts else { return false };
    date_ranges.iter().all(|(r, negated)| {
        let within = r.start.map_or(true, |s| ts >= s) && r.end.map_or(true, |e| ts < e);
        within != *negated
    })
}

/// `count`, saturating at `cap`, normalized to `0.0..=1.0`.
fn saturate(count: i64, cap: i64) -> f64 {
    if cap <= 0 {
        return 0.0;
    }
    count.clamp(0, cap) as f64 / cap as f64
}

/// Inbox-vs-Archive-vs-Spam prior, from the mailbox's own name (case-
/// insensitive substring match — `mailboxes.attributes` carries only
/// `\Noselect` in this build, see `imap::folders::store_folders`'s own
/// "richer attribute capture can follow when sync needs it" note, so a
/// special-use IMAP attribute like `\Junk`/`\Archive` is not yet available
/// to read here).
fn folder_prior(mailbox_name: &str) -> f64 {
    let lower = mailbox_name.to_lowercase();
    if lower.contains("spam") || lower.contains("junk") {
        0.05
    } else if lower.contains("trash") || lower.contains("deleted") {
        0.02
    } else if lower == "inbox" || lower.ends_with("/inbox") {
        1.0
    } else if lower.contains("archive") {
        0.6
    } else if lower.contains("sent") {
        0.5
    } else {
        0.4
    }
}

/// Heuristic bulk-sender detection from an already-lowercased address/
/// display-name — see [`AUTOMATED_SENDER_KEYWORDS`]'s doc comment for why
/// this is implemented for real rather than defaulted like `ai_priority`.
fn detect_bulk_sender(addr: Option<&str>, name: Option<&str>) -> (bool, bool) {
    let haystack = format!("{} {}", addr.unwrap_or(""), name.unwrap_or(""));
    let is_automated = AUTOMATED_SENDER_KEYWORDS
        .iter()
        .any(|k| haystack.contains(k));
    let is_newsletter = NEWSLETTER_SENDER_KEYWORDS
        .iter()
        .any(|k| haystack.contains(k));
    (is_newsletter, is_automated)
}

#[cfg(test)]
mod tests;
