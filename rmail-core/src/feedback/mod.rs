//! The implicit-feedback learning loop's local log (prd.md, "Personalization
//! & Implicit-Feedback Learning Loop"; task 64): which results a search
//! showed, at what position, ranked by exactly which feature vector, and what
//! the user then did with them.
//!
//! This module closes the loop that makes relevance improve with use. Task
//! 65's offline trainer reads what is written here, turns it into
//! position-bias-corrected pairwise labels, and hot-swaps a model on a
//! measured NDCG win. Everything below exists to make that replay *exact*
//! rather than approximate.
//!
//! # Why the logged feature vector is the ranker's own, never a re-derivation
//!
//! The single most tempting shortcut here is to log the query and the
//! message ids and re-extract features at training time. It does not work,
//! and the failure is silent. [`crate::features::FeatureExtractor`] reads the
//! *current* corpus: BM25 scores move as the index grows, `sender_affinity`
//! and `thread_activity` move as mail arrives, `recency_decay` moves by
//! definition, and `is_unread` flips the moment the user opens the message —
//! which, for a clicked result, is precisely the row the trainer cares most
//! about. Re-derivation would train the model on features that *followed*
//! the click it is trying to predict.
//!
//! So [`Impression::features`] is the vector the L1 ranker actually scored,
//! carried straight from [`crate::features::CandidateFeatures`] through
//! [`encode_features`] into the row. [`Impression::l1_score`] is the score it
//! actually produced, stored beside it so a replay can be *checked* rather
//! than trusted, and `search_log.intent` is stored too — see [`intent_name`]
//! for why a vector without its intent replays to the wrong number.
//!
//! # Opt-out means nothing is written, not that something is filtered
//!
//! prd.md: "Logging is strictly opt-outable (`search.learning = false`); it
//! is local telemetry, never transmitted." [`FeedbackStore`] takes that
//! literally. With learning off, [`FeedbackStore::new_query_id`] returns
//! `None` — so a caller never even builds an impression — and both write
//! paths return `Ok(0)` before touching the database. There is no "log then
//! filter" stage that a later refactor could accidentally drop, and no
//! partially-populated table to explain to a user who opted out.
//!
//! Local-only is a structural property, not a policy toggle: nothing in this
//! module, and nothing that reads these tables, has a network client. The
//! only RPC over this data (`SearchService.LogFeedback`) writes *in*; there
//! is deliberately no RPC that reads any of it back out.
//!
//! # Logging never slows down or fails a search
//!
//! Two rules, both structural rather than aspirational:
//!
//! - **Off the critical path.** `query_id`s are minted in-process
//!   ([`new_query_id`]) rather than allocated by SQLite, so nothing has to be
//!   written before the first hit streams; `rmaild::search_service` calls
//!   [`FeedbackStore::log_query`] only after the response stream has already
//!   been closed.
//! - **Never fatal.** Every write here goes through [`crate::storage::Database`],
//!   which is `spawn_blocking`-backed (rusqlite is synchronous), and a
//!   failure is a `warn`-level event at the call site rather than an error a
//!   searcher ever sees. A lost log line costs a training example; a failed
//!   search costs the feature.
//!
//! # Bounded growth
//!
//! An impression is ~0.8 kB of serialized feature vector and a search logs a
//! whole page of them, which makes this the fastest-growing table in the
//! database if left alone. [`FeedbackStore::prune`] applies
//! [`crate::config::FeedbackConfig`]'s age and query-count bounds, chunked so
//! a first pass over a backlog does not hold the single writer connection for
//! the whole delete — the same shape [`crate::events::EventLog::prune`] uses.

pub(crate) mod repo;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::FeedbackConfig;
use crate::error::Error;
use crate::features::FeatureVector;
use crate::index::extract::normalize;
use crate::query::Intent;
use crate::storage::Database;

/// The version stamped into every [`encode_features`] envelope.
///
/// Bump this when the *encoding* changes in a way a decoder must branch on —
/// not when a feature is added or removed. See [`EncodedFeatures`] for the
/// difference and why it matters to task 65.
pub const FEATURE_FORMAT_VERSION: u32 = 1;

/// Largest number of impressions one [`FeedbackStore::log_query`] call
/// persists.
///
/// A caller already bounds this by construction (impressions come from a
/// presented page, itself capped by `search.default_limit`/`top_k_rerank`),
/// so this is a defensive ceiling on one transaction's size rather than a
/// tuning knob — the same role `fuse::MAX_META_FETCH` and
/// `features::MAX_META_FETCH` play for their own batches. Extra impressions
/// are dropped from the tail (the lowest-ranked, least informative end) and
/// logged, never silently.
pub const MAX_IMPRESSIONS_PER_QUERY: usize = 200;

/// Largest number of actions one [`FeedbackStore::log_actions`] call accepts.
///
/// This one is a real limit rather than a defensive ceiling: the actions on
/// the other side of it arrive from a client over gRPC, so it bounds what an
/// unbounded or buggy caller can write in a single request. Exceeding it is
/// an [`Error::InvalidArgument`], not a silent truncation — dropping user
/// actions on the floor would corrupt the pairwise labels task 65 derives
/// from them (a "skipped" result that was actually opened is a wrong label,
/// not a missing one).
pub const MAX_ACTIONS_PER_REQUEST: usize = 512;

/// What the user did with a result (prd.md's own vocabulary:
/// `open|reply|archive|dwell|scroll_past`).
///
/// A closed enum rather than a free string, so the five things the trainer
/// knows how to weight are the five things that can reach the table. The
/// column itself is open `TEXT` — see `V30__search_feedback.sql` for why that
/// matches this schema's existing convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionKind {
    /// The result was opened. prd.md's baseline positive signal.
    Open,
    /// The user replied to the message. The strongest positive.
    Reply,
    /// Archived straight from the result list — prd.md: "a mild negative for
    /// that query," not a neutral event.
    Archive,
    /// Time spent on the message, in [`Action::dwell_ms`]. prd.md ranks
    /// "reply/long-dwell > open > hover", so the duration is the signal and a
    /// `Dwell` without one is rejected.
    Dwell,
    /// Skipped over on the way to something lower. What makes a
    /// `clicked ≻ skipped-above` pair a *pair* rather than a lone positive.
    ScrollPast,
}

impl ActionKind {
    /// Every kind, for exhaustive iteration in tests and tooling.
    pub const ALL: [ActionKind; 5] = [
        ActionKind::Open,
        ActionKind::Reply,
        ActionKind::Archive,
        ActionKind::Dwell,
        ActionKind::ScrollPast,
    ];

    /// The stable name persisted in `search_action.action`. Never Rust's
    /// `Debug` capitalization: this string is on disk and task 65 parses it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ActionKind::Open => "open",
            ActionKind::Reply => "reply",
            ActionKind::Archive => "archive",
            ActionKind::Dwell => "dwell",
            ActionKind::ScrollPast => "scroll_past",
        }
    }

    /// Parse [`ActionKind::as_str`]'s output back, or `None` for anything
    /// else.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == raw)
    }
}

/// The stable lowercase name for an [`Intent`], as stored in
/// `search_log.intent`.
///
/// Stored because the L1 score is not a function of the feature vector alone:
/// [`crate::rank::l1::Weights::score`] zeroes the `is_newsletter`/
/// `is_automated` weights under [`Intent::Navigational`], so replaying a
/// logged vector under the wrong intent reproduces a different number than
/// the user was shown. That would be invisible — a plausible score, quietly
/// mistrained.
///
/// Defined here rather than as a method on [`Intent`] itself, following the
/// convention `features::vector`'s `source_serde` documents: a downstream
/// consumer that needs one specific capability from a shared type adds it
/// locally instead of growing the shared type.
#[must_use]
pub const fn intent_name(intent: Intent) -> &'static str {
    match intent {
        Intent::Navigational => "navigational",
        Intent::Exploratory => "exploratory",
        Intent::Lookup => "lookup",
    }
}

/// Parse [`intent_name`]'s output back — task 65's side of the same mapping.
#[must_use]
pub fn parse_intent(raw: &str) -> Option<Intent> {
    match raw {
        "navigational" => Some(Intent::Navigational),
        "exploratory" => Some(Intent::Exploratory),
        "lookup" => Some(Intent::Lookup),
        _ => None,
    }
}

/// The wire format of `search_impression.features`: a version tag plus the
/// typed vector.
///
/// # Why JSON, and why an envelope around it
///
/// [`FeatureVector`] is already `serde`-serializable with a deliberately
/// stable field order (see its own module docs), and `serde_json` round-trips
/// finite `f64` exactly — so `decode_features(encode_features(v)) == v`
/// holds bit for bit, which is what "exact replay" has to mean for a model
/// whose input is those bits. A packed array of floats would be smaller and
/// would also be *positional*: adding a feature in the middle would silently
/// reinterpret every historical row. Field names make an added feature a
/// `#[serde(default)]` away and a removed one a no-op, which is the migration
/// story task 65 actually needs.
///
/// The envelope carries [`EncodedFeatures::version`] so a decoder can tell
/// "an encoding I understand" from "an encoding from a future rmail" without
/// guessing from the payload's shape. It tracks the *encoding*, not the
/// feature set — a row whose `features` object is missing a field added later
/// is still version 1 and still decodable, which is exactly the case that
/// would otherwise force a version bump on every feature addition and make
/// the number useless.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodedFeatures {
    /// [`FEATURE_FORMAT_VERSION`] at the time of writing.
    pub version: u32,
    /// The vector the ranker scored, verbatim.
    pub features: FeatureVector,
}

/// Serialize a feature vector for `search_impression.features`.
///
/// # Errors
///
/// [`Error::Internal`] if serialization fails. In practice it cannot:
/// `serde_json`'s only failure mode for this shape is a non-finite `f64`, and
/// `features::vector`'s own `finite` helper already routes every arithmetic
/// path in extraction away from producing one. It is surfaced as a `Result`
/// rather than swallowed so a future feature that forgets that guarantee
/// fails loudly at its own call site instead of writing an unreadable row.
pub fn encode_features(features: &FeatureVector) -> Result<Vec<u8>, Error> {
    let envelope = EncodedFeatures {
        version: FEATURE_FORMAT_VERSION,
        features: features.clone(),
    };
    serde_json::to_vec(&envelope)
        .map_err(|error| Error::internal(format!("serializing a feature vector: {error}")))
}

/// Decode a `search_impression.features` blob — the replay half of
/// [`encode_features`], and the entry point task 65 reads a logged impression
/// through.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the blob is not the documented envelope, or
/// carries a [`EncodedFeatures::version`] this build does not understand. A
/// newer version is rejected rather than best-effort parsed: silently
/// training on a misread vector is worse than skipping the row.
pub fn decode_features(blob: &[u8]) -> Result<FeatureVector, Error> {
    let envelope: EncodedFeatures = serde_json::from_slice(blob).map_err(|error| {
        Error::invalid_argument(format!("malformed impression feature vector: {error}"))
    })?;
    if envelope.version != FEATURE_FORMAT_VERSION {
        return Err(Error::invalid_argument(format!(
            "impression feature vector is format version {}, this build reads {}",
            envelope.version, FEATURE_FORMAT_VERSION
        )));
    }
    Ok(envelope.features)
}

/// SHA-256 over the normalized query text — `search_log.norm_hash`.
///
/// Normalization is [`crate::index::extract::normalize`], the same NFC/
/// whitespace folding the indexes agree on, so "the same search typed twice"
/// groups even when the two differ by a double space or a decomposed accent.
/// Reusing the indexes' definition rather than inventing a query-local one is
/// deliberate: two normalizers that disagree would make the grouping key mean
/// something subtly different from what the retrieval path considers the same
/// text.
#[must_use]
pub fn norm_hash(raw_query: &str) -> Vec<u8> {
    Sha256::digest(normalize(raw_query).to_lowercase().as_bytes()).to_vec()
}

/// Mint a `query_id` for a search that is about to run.
///
/// # Why not a rowid
///
/// The id has to be on every `SearchHit` as it streams, so a client can
/// attribute an action back to the query that produced it. Letting SQLite
/// assign it would mean writing the `search_log` row *before* the first hit
/// — a synchronous INSERT on the single writer connection, inside prd.md's
/// 30 ms first-paint budget and queued behind whatever sync batch holds the
/// writer. Minting in-process moves every write after the response.
///
/// # Why this is unique in practice
///
/// The digest covers a process-lifetime monotonic counter (unique within a
/// process), the process id, and the wall clock in nanoseconds (which
/// separates two processes that share a pid across a reboot). The result is
/// truncated to 63 bits and forced non-zero, since `0` is proto3's default
/// for `SearchHit.query_id` and therefore means "this query was not logged."
///
/// A collision would surface as a `PRIMARY KEY` conflict on insert, which
/// [`FeedbackStore::log_query`] reports as a dropped log line rather than a
/// failed search — see its docs.
#[must_use]
pub fn new_query_id() -> i64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_nanos();

    let mut hasher = Sha256::new();
    hasher.update(counter.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(nanos.to_le_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    // Mask to 63 bits: `search_log.query_id` is a SQLite INTEGER and the wire
    // field is an int64, so a negative id would round-trip fine but read as
    // nonsense in every log line and query that ever touches it.
    let id = i64::from_le_bytes(bytes) & i64::MAX;
    // `0` is the "not logged" sentinel on the wire; never mint it.
    if id == 0 {
        1
    } else {
        id
    }
}

/// One query's row in `search_log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryRecord {
    /// The id minted by [`new_query_id`] and already streamed to the client.
    pub query_id: i64,
    /// The account the search was scoped to; `None` for every account.
    pub account_id: Option<i64>,
    /// The query text as it reached the planner, verbatim.
    pub raw_query: String,
    /// The intent the ranker actually scored under — see [`intent_name`].
    pub intent: Intent,
    /// When the search was issued (unix seconds).
    pub issued_at: i64,
}

/// One result the user was shown, and the exact vector it was ranked by.
#[derive(Debug, Clone, PartialEq)]
pub struct Impression {
    /// `messages.id` — never the IMAP UID; see the migration's own note.
    pub message_id: i64,
    /// 1-based rank in the page the user saw, top result first.
    pub position: u32,
    /// The vector [`crate::rank::Ranker`] scored this candidate from.
    pub features: FeatureVector,
    /// The Stage 4 score it produced.
    pub l1_score: f64,
    /// The Stage 5 rerank score, when an L2 stage ran (task 51). `None`
    /// means "no reranker ran", which is not the same fact as a score of 0.
    pub l2_score: Option<f64>,
}

/// One thing the user did with a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action {
    /// `messages.id` the action was taken on.
    pub message_id: i64,
    /// Which action.
    pub kind: ActionKind,
    /// Milliseconds dwelled. Required for [`ActionKind::Dwell`] and rejected
    /// for every other kind — a dwell without a duration carries no signal,
    /// and a duration on an `Open` would be a second, competing definition of
    /// the same measurement.
    pub dwell_ms: Option<i64>,
    /// When it happened (unix seconds).
    pub at: i64,
}

/// What one [`FeedbackStore::prune`] pass removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pruned {
    /// `search_log` rows deleted. Their impressions and actions went with
    /// them via `ON DELETE CASCADE`.
    pub queries: u64,
}

/// The local feedback log.
///
/// Cheap to clone: every clone shares one database handle. Construction takes
/// the `learning` switch and retention bounds by value so a handed-out clone
/// cannot observe a different policy than the one it was built with — the
/// opt-out is not something a caller can be talked out of consulting.
#[derive(Debug, Clone)]
pub struct FeedbackStore {
    db: Database,
    enabled: bool,
    retention: FeedbackConfig,
}

impl FeedbackStore {
    /// Build a store over `db`.
    ///
    /// `enabled` is `search.learning`. When it is `false` this store writes
    /// nothing, ever — see the module docs.
    #[must_use]
    pub fn new(db: Database, enabled: bool, retention: FeedbackConfig) -> Self {
        Self {
            db,
            enabled,
            retention,
        }
    }

    /// Whether implicit-feedback logging is on (`search.learning`).
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Mint a `query_id` for a search about to run, or `None` when learning
    /// is off.
    ///
    /// Returning `None` rather than an unused id is what makes the opt-out
    /// structural: a caller that has no id has no impression to build, no
    /// value to stamp on a `SearchHit`, and nothing to hand [`Self::log_query`].
    #[must_use]
    pub fn new_query_id(&self) -> Option<i64> {
        self.enabled.then(new_query_id)
    }

    /// Persist one query and the page it showed, in a single transaction.
    ///
    /// Returns how many impressions were written — `0` when learning is off,
    /// when `impressions` is empty (a query that showed nothing has nothing
    /// to learn from, and a bare `search_log` row would only be a record of
    /// what was searched for), or when the id collided with an existing row.
    ///
    /// Impressions past [`MAX_IMPRESSIONS_PER_QUERY`] are dropped from the
    /// tail and the drop is logged.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if a feature vector cannot be serialized, or a
    /// mapped storage error. Callers on a search path must treat *any* error
    /// here as a warning, never as a failed search — see the module docs.
    #[tracing::instrument(
        skip(self, record, impressions),
        fields(
            query_id = record.query_id,
            intent = intent_name(record.intent),
            impressions = impressions.len(),
        ),
        // `warn`, not the house-default `err` (which emits at ERROR): the
        // module docs' contract is that a failure to log is a warning, never
        // something a searcher sees — and the call site
        // (`rmaild::search_service::log_page`) already warns. Leaving this at
        // ERROR would report the same non-event twice, at two severities, and
        // put an operator-alerting line in the log for a lost training
        // example.
        err(level = "warn"),
    )]
    pub async fn log_query(
        &self,
        record: QueryRecord,
        mut impressions: Vec<Impression>,
    ) -> Result<usize, Error> {
        if !self.enabled {
            tracing::debug!("search.learning is off; not writing a search_log row");
            return Ok(0);
        }
        if impressions.is_empty() {
            return Ok(0);
        }
        if impressions.len() > MAX_IMPRESSIONS_PER_QUERY {
            tracing::warn!(
                shown = impressions.len(),
                cap = MAX_IMPRESSIONS_PER_QUERY,
                "impression batch over the per-query cap; dropping the lowest-ranked tail"
            );
            impressions.truncate(MAX_IMPRESSIONS_PER_QUERY);
        }

        // Serialized before the transaction opens: encoding is pure CPU work
        // and a failure here must not leave a half-written query behind.
        let rows = impressions
            .iter()
            .map(|impression| {
                Ok(repo::ImpressionRow {
                    message_id: impression.message_id,
                    position: i64::from(impression.position),
                    features: encode_features(&impression.features)?,
                    l1_score: impression.l1_score,
                    l2_score: impression.l2_score,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;

        let log_row = repo::LogRow {
            query_id: record.query_id,
            account_id: record.account_id,
            raw_query: record.raw_query.clone(),
            norm_hash: norm_hash(&record.raw_query),
            intent: intent_name(record.intent),
            issued_at: record.issued_at,
            result_count: i64::try_from(rows.len()).unwrap_or(i64::MAX),
        };

        let written = self
            .db
            .write(move |conn| Ok(repo::insert_query(conn, &log_row, &rows)))
            .await?;

        match written {
            Ok(count) => Ok(count),
            // A minted id colliding with a live row is astronomically
            // unlikely (63 bits over a counter, pid and nanosecond clock) but
            // it is a *log* failure, not a search failure: the page was
            // already served, so it is reported and dropped rather than
            // retried under a fresh id — the client is holding the colliding
            // one, and re-minting would leave it reporting actions against an
            // id nothing was written under.
            //
            // The client keeping that id has one consequence worth stating
            // plainly: its later `LogFeedback` calls resolve to the *other*
            // query's row, so its actions would be attributed there — except
            // that `log_actions` also checks each message against that
            // query's own impressions, so in practice the mismatched batch is
            // rejected rather than mis-filed.
            Err(err) if repo::is_unique_violation(&err) => {
                tracing::warn!(
                    query_id = record.query_id,
                    "minted query_id collided with an existing search_log row; impressions dropped"
                );
                Ok(0)
            }
            Err(err) => Err(Error::from(crate::StorageError::from(err))),
        }
    }

    /// Persist actions against an already-logged query.
    ///
    /// Returns how many rows were written; `0` when learning is off or
    /// `actions` is empty.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if `query_id` is zero, the batch exceeds
    /// [`MAX_ACTIONS_PER_REQUEST`], an action is malformed (see
    /// [`validate_action`]), or an action names a message this query never
    /// showed. [`Error::NotFound`] if `query_id` names no logged query —
    /// including one already dropped by retention. Otherwise a mapped storage
    /// error.
    // `query_id` is deliberately *not* redeclared in `fields(...)`: an
    // explicitly declared field of the same name as an argument suppresses
    // the argument's own auto-recording, and declaring it with no value would
    // leave every span carrying an empty `query_id` — the one field that
    // makes these log lines correlatable.
    #[tracing::instrument(skip(self, actions), fields(actions = actions.len()), err)]
    pub async fn log_actions(&self, query_id: i64, actions: &[Action]) -> Result<usize, Error> {
        if !self.enabled {
            tracing::debug!("search.learning is off; not writing search_action rows");
            return Ok(0);
        }
        if query_id == 0 {
            return Err(Error::invalid_argument(
                "query_id is required; take it from SearchHit.query_id",
            ));
        }
        if actions.len() > MAX_ACTIONS_PER_REQUEST {
            return Err(Error::invalid_argument(format!(
                "at most {MAX_ACTIONS_PER_REQUEST} actions per request, got {}",
                actions.len()
            )));
        }
        for action in actions {
            validate_action(action)?;
        }
        if actions.is_empty() {
            return Ok(0);
        }

        let rows: Vec<repo::ActionRow> = actions
            .iter()
            .map(|action| repo::ActionRow {
                message_id: action.message_id,
                action: action.kind.as_str(),
                dwell_ms: action.dwell_ms,
                at: action.at,
            })
            .collect();

        let outcome = self
            .db
            .write(move |conn| repo::insert_actions(conn, query_id, &rows))
            .await?;

        match outcome {
            repo::ActionOutcome::Written(count) => Ok(count),
            // Distinguished so a client can tell "your query_id is stale —
            // retention dropped it, or learning was off when you searched"
            // from "the database is broken."
            repo::ActionOutcome::UnknownQuery => Err(Error::not_found(format!(
                "no logged search with query_id {query_id}"
            ))),
            // An action about a result the query never showed has no position
            // and no feature vector to attribute itself to — see
            // `repo::insert_actions`' own docs for why this is a rejection
            // rather than a row.
            repo::ActionOutcome::NotShown(message_id) => Err(Error::invalid_argument(format!(
                "search {query_id} never showed message {message_id}, so there is nothing \
                 for an action on it to mean"
            ))),
        }
    }

    /// Apply retention, returning what was dropped.
    ///
    /// Both of [`FeedbackConfig`]'s bounds apply and whichever bites first
    /// wins. Deletes are chunked so a first pass over a backlog does not hold
    /// the single writer connection for the whole sweep — the same reasoning
    /// [`crate::events::EventLog::prune`] documents.
    ///
    /// Runs regardless of `search.learning`: turning learning off should
    /// *retire* what was already collected, not freeze it on disk forever.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self), err)]
    pub async fn prune(&self) -> Result<Pruned, Error> {
        self.prune_at(Utc::now()).await
    }

    /// [`Self::prune`] against an explicit reference instant.
    ///
    /// Exists so the age bound is testable without sleeping or backdating the
    /// system clock — the same seam `features::FeatureExtractor::extract_at`
    /// and `query::QueryPlanner::plan_at` already provide for the same
    /// reason.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn prune_at(&self, now: DateTime<Utc>) -> Result<Pruned, Error> {
        let max_queries = i64::try_from(self.retention.max_queries).unwrap_or(i64::MAX);
        // Saturating, not `.ok()`: an out-of-range value mapping to "no
        // horizon" would read as *unlimited*, turning a config typo into
        // unbounded disk growth. `GrpcEvents`' own conversion makes the
        // identical call for the identical reason.
        let cutoff = now
            .timestamp()
            .saturating_sub(i64::from(self.retention.retention_days).saturating_mul(24 * 60 * 60));

        let mut queries = 0u64;
        loop {
            let removed = self
                .db
                .write(move |conn| repo::prune_chunk(conn, cutoff, max_queries, PRUNE_CHUNK))
                .await?;
            queries = queries.saturating_add(removed as u64);
            if removed < PRUNE_CHUNK as usize {
                break;
            }
        }

        if queries > 0 {
            tracing::info!(queries, "pruned the search feedback log");
        }
        Ok(Pruned { queries })
    }

    /// How many queries the log currently holds.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn query_count(&self) -> Result<i64, Error> {
        Ok(self.db.read(repo::count_queries).await?)
    }
}

/// The largest number of `search_log` rows one prune pass deletes at a time.
///
/// Smaller than [`crate::events::EventLog`]'s 10 000 because each row here
/// cascades into a whole page of `search_impression` rows, so one chunk is
/// already tens of thousands of deletes against the writer connection.
const PRUNE_CHUNK: i64 = 1_000;

/// Longest dwell this accepts, in milliseconds: 24 hours.
///
/// Not a guess about human attention — it is a bound on a number a *client*
/// supplies and task 65 will weight labels by. prd.md ranks a long dwell
/// above a plain open, so a single forged or buggy dwell of a thousand years
/// would dominate every honest signal in the corpus. A tab left open
/// overnight is already an outlier a sane client should not report; anything
/// past a day is a bug or an attack, and neither is evidence about relevance.
pub const MAX_DWELL_MS: i64 = 24 * 60 * 60 * 1_000;

/// Reject an action that carries no usable signal, or a value a caller should
/// not be able to put in the training corpus.
///
/// # Errors
///
/// [`Error::InvalidArgument`] with the specific violation.
fn validate_action(action: &Action) -> Result<(), Error> {
    // `at` is a client-supplied timestamp that task 65 reads for recency and
    // propensity weighting, so a negative one (pre-1970: not a time any
    // feedback was generated) is rejected outright rather than stored and
    // reasoned about later. No upper bound is imposed here: this function is
    // the domain rule, and "how far into the future is too far" depends on a
    // clock it deliberately does not read — the gRPC boundary substitutes the
    // daemon's own clock for the unset case, which is what a local client
    // should be sending anyway.
    if action.at < 0 {
        return Err(Error::invalid_argument(format!(
            "action timestamp must not be negative, got {}",
            action.at
        )));
    }
    match (action.kind, action.dwell_ms) {
        (ActionKind::Dwell, None) => Err(Error::invalid_argument(
            "a dwell action must carry dwell_ms; how long is the whole signal",
        )),
        (ActionKind::Dwell, Some(ms)) if ms < 0 => Err(Error::invalid_argument(format!(
            "dwell_ms must not be negative, got {ms}"
        ))),
        (ActionKind::Dwell, Some(ms)) if ms > MAX_DWELL_MS => Err(Error::invalid_argument(
            format!("dwell_ms must be at most {MAX_DWELL_MS} (24h), got {ms}"),
        )),
        (kind, Some(_)) if kind != ActionKind::Dwell => Err(Error::invalid_argument(format!(
            "dwell_ms is only meaningful on a dwell action, not {}",
            kind.as_str()
        ))),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
