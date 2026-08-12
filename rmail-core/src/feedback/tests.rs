//! What task 64 owes for the feedback log: impressions carry the position
//! *and* the exact feature vector the ranker scored; actions cover prd.md's
//! whole vocabulary; the opt-out writes nothing at all; and growth is
//! bounded.
//!
//! Every opt-out test here asserts on the **absence of rows**, read back with
//! raw SQL, not on a return value. A `FeedbackStore` that dutifully wrote and
//! then filtered would satisfy any assertion about what `log_query` returned
//! while leaving the user's search history on disk, which is the exact
//! failure the acceptance criterion names.
//!
//! The gRPC half — that `SearchService.LogFeedback` is reachable, scoped, and
//! maps these errors to the right `Status` codes, and that a real search
//! actually logs the vector its ranker used — lives in
//! `rmaild/tests/search_service.rs`, against an in-process tonic server.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::TimeZone;

use super::*;
use crate::config::FeedbackConfig;
use crate::error::ErrorReason;
use crate::features::MatchField;
use crate::rank::l1::L1Ranker;
use crate::repo;
use crate::retrieve::Source;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Retention wide enough that no test hits it by accident — the pruning
/// tests build their own bounds.
fn no_prune() -> FeedbackConfig {
    FeedbackConfig {
        retention_days: 36_500,
        max_queries: 1_000_000,
    }
}

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-feedback-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open temp db");
        let account_id = db
            .with_write(move |conn| {
                repo::insert_account(
                    conn,
                    &repo::NewAccount {
                        name: format!("acct-{n}"),
                        ..Default::default()
                    },
                )
            })
            .expect("seed account");
        Self {
            db,
            path,
            account_id,
        }
    }

    fn store(&self, enabled: bool) -> FeedbackStore {
        FeedbackStore::new(self.db.clone(), enabled, no_prune())
    }

    fn store_with(&self, enabled: bool, retention: FeedbackConfig) -> FeedbackStore {
        FeedbackStore::new(self.db.clone(), enabled, retention)
    }

    /// Raw row counts, read with SQL that never goes through the store — the
    /// only assertion that can distinguish "wrote nothing" from "wrote and
    /// filtered."
    fn counts(&self) -> (i64, i64, i64) {
        self.db
            .with_read(|conn| {
                Ok((
                    conn.query_row("SELECT count(*) FROM search_log", [], |r| r.get(0))?,
                    conn.query_row("SELECT count(*) FROM search_impression", [], |r| r.get(0))?,
                    conn.query_row("SELECT count(*) FROM search_action", [], |r| r.get(0))?,
                ))
            })
            .expect("count rows")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// A feature vector with a distinct, non-default value in every field that
/// can hold one, so a round trip that dropped or transposed a field cannot
/// pass by accident.
fn rich_vector() -> FeatureVector {
    FeatureVector {
        bm25_subject: 12.5,
        bm25_body: 3.25,
        bm25_from: 1.125,
        bm25_attach: 0.5,
        exact_phrase_hit: true,
        term_coverage: 0.875,
        proximity_min_span: Some(7),
        best_match_field: MatchField::Subject,
        fuzzy_score: 0.4321,
        cos_max_chunk: 0.912_345_678_9,
        cos_mean_chunk: 0.654_321,
        rrf_score: 0.033_333_333_333_333_33,
        num_sources_hit: 4,
        best_source: Source::Dense,
        sender_affinity: 0.75,
        user_replied_thread: true,
        prior_opens_from_sender: 0.25,
        thread_activity: 0.6,
        age_days: Some(12.345),
        recency_decay: 0.678,
        matches_date_intent: true,
        is_unread: true,
        is_flagged: true,
        is_pinned: true,
        ai_priority: 0.9,
        has_tag_match: true,
        folder_prior: 1.0,
        has_attachment_match: true,
        is_thread_root: true,
        thread_size: 9,
        msg_length: 4096,
        sender_reputation: 0.42,
        is_newsletter: true,
        is_automated: true,
    }
}

fn record(fx: &Fixture, query_id: i64, raw: &str) -> QueryRecord {
    QueryRecord {
        query_id,
        account_id: Some(fx.account_id),
        raw_query: raw.to_owned(),
        intent: Intent::Navigational,
        issued_at: 1_700_000_000,
    }
}

fn impression(message_id: i64, position: u32) -> Impression {
    Impression {
        message_id,
        position,
        features: rich_vector(),
        l1_score: 1.5 - f64::from(position),
        l2_score: None,
    }
}

// ---------------------------------------------------------------------------
// Impression logging: position + the exact vector the ranker scored
// ---------------------------------------------------------------------------

#[tokio::test]
async fn logging_a_query_persists_its_page_with_positions_and_features() {
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");

    let written = store
        .log_query(
            record(&fx, query_id, "from:alice invoice"),
            vec![impression(11, 1), impression(22, 2), impression(33, 3)],
        )
        .await
        .expect("log");
    assert_eq!(written, 3);

    let rows: Vec<(i64, i64, Vec<u8>, f64)> = fx
        .db
        .with_read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT message_id, position, features, l1_score FROM search_impression
                     WHERE query_id = ?1 ORDER BY position",
            )?;
            let rows = stmt
                .query_map([query_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })
        .expect("read impressions");

    assert_eq!(rows.len(), 3);
    for (index, (message_id, position, blob, l1)) in rows.iter().enumerate() {
        let rank = index as i64 + 1;
        assert_eq!(*position, rank, "position is the 1-based rank");
        assert_eq!(*message_id, rank * 11);
        assert!(
            (*l1 - (1.5 - rank as f64)).abs() < f64::EPSILON,
            "the score the ranker produced is stored beside the vector"
        );
        assert_eq!(
            decode_features(blob).expect("decode"),
            rich_vector(),
            "the stored vector is the one the ranker scored, field for field"
        );
    }

    let (log_rows, impressions, actions) = fx.counts();
    assert_eq!((log_rows, impressions, actions), (1, 3, 0));
}

#[tokio::test]
async fn the_search_log_row_records_the_query_intent_and_result_count() {
    // The intent is not cosmetic: `rank::l1` zeroes the newsletter/automated
    // weights under a navigational intent, so replaying a stored vector
    // without its intent reproduces a *different* score than the user saw.
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");
    let mut rec = record(&fx, query_id, "  Office   Move  ");
    rec.intent = Intent::Exploratory;

    store
        .log_query(rec, vec![impression(1, 1), impression(2, 2)])
        .await
        .expect("log");

    let (raw, hash, intent, issued, count, account): (
        String,
        Vec<u8>,
        Option<String>,
        i64,
        Option<i64>,
        Option<i64>,
    ) = fx
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT raw_query, norm_hash, intent, issued_at, result_count, account_id
                 FROM search_log WHERE query_id = ?1",
                [query_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
        })
        .expect("read log row");

    assert_eq!(raw, "  Office   Move  ", "the query is stored verbatim");
    assert_eq!(
        hash,
        norm_hash("office move"),
        "the hash is over the normalized form, so whitespace/case repeats group"
    );
    assert_eq!(intent.as_deref(), Some("exploratory"));
    assert_eq!(issued, 1_700_000_000);
    assert_eq!(count, Some(2));
    assert_eq!(account, Some(fx.account_id));
}

#[tokio::test]
async fn a_query_that_showed_nothing_writes_no_row_at_all() {
    // A `search_log` row with no impressions is a record of what the user
    // searched for and nothing the trainer can use — the one thing this table
    // must not become.
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");

    let written = store
        .log_query(record(&fx, query_id, "nothing matches this"), Vec::new())
        .await
        .expect("log");

    assert_eq!(written, 0);
    assert_eq!(fx.counts(), (0, 0, 0));
}

#[tokio::test]
async fn an_impression_batch_over_the_cap_is_truncated_not_rejected() {
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");

    let oversized: Vec<Impression> = (1..=(MAX_IMPRESSIONS_PER_QUERY + 25))
        .map(|i| impression(i as i64, i as u32))
        .collect();
    let written = store
        .log_query(record(&fx, query_id, "wide page"), oversized)
        .await
        .expect("log");

    assert_eq!(written, MAX_IMPRESSIONS_PER_QUERY);
    let (_, impressions, _) = fx.counts();
    assert_eq!(impressions, MAX_IMPRESSIONS_PER_QUERY as i64);
}

#[tokio::test]
async fn a_page_and_its_query_row_land_together_or_not_at_all() {
    // The transaction, observed rather than asserted about: a duplicate
    // `query_id` fails the `search_log` insert, and the impressions that were
    // to follow it must not be visible either.
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");

    store
        .log_query(record(&fx, query_id, "first"), vec![impression(1, 1)])
        .await
        .expect("first log");

    let written = store
        .log_query(
            record(&fx, query_id, "collided"),
            vec![impression(2, 1), impression(3, 2)],
        )
        .await
        .expect("a collision is a dropped log line, never an error to the searcher");

    assert_eq!(written, 0);
    assert_eq!(
        fx.counts(),
        (1, 1, 0),
        "nothing from the collided call survived"
    );
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_action_in_the_vocabulary_round_trips() {
    // prd.md's own list: open | reply | archive | dwell | scroll_past. All
    // five, driven through the real store, read back with raw SQL.
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");
    // One impression per action: an action may only name a message its query
    // actually showed.
    let page: Vec<Impression> = (1..=ActionKind::ALL.len())
        .map(|i| impression(i as i64, i as u32))
        .collect();
    store
        .log_query(record(&fx, query_id, "q"), page)
        .await
        .expect("log");

    let actions: Vec<Action> = ActionKind::ALL
        .into_iter()
        .enumerate()
        .map(|(i, kind)| Action {
            message_id: i as i64 + 1,
            kind,
            dwell_ms: (kind == ActionKind::Dwell).then_some(4_200),
            at: 1_700_000_100 + i as i64,
        })
        .collect();

    let written = store
        .log_actions(query_id, &actions)
        .await
        .expect("log actions");
    assert_eq!(written, ActionKind::ALL.len());

    let stored: Vec<(String, Option<i64>)> = fx
        .db
        .with_read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT action, dwell_ms FROM search_action WHERE query_id = ?1 ORDER BY at",
            )?;
            let rows = stmt
                .query_map([query_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })
        .expect("read actions");

    let expected: Vec<(String, Option<i64>)> = ActionKind::ALL
        .into_iter()
        .map(|kind| {
            (
                kind.as_str().to_owned(),
                (kind == ActionKind::Dwell).then_some(4_200),
            )
        })
        .collect();
    assert_eq!(stored, expected);
}

#[tokio::test]
async fn the_same_result_can_be_acted_on_more_than_once() {
    // Repetition is signal here, not duplication: opened, then archived, is
    // two observations about one result. A unique constraint would silently
    // collapse them.
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");
    store
        .log_query(record(&fx, query_id, "q"), vec![impression(7, 1)])
        .await
        .expect("log");

    store
        .log_actions(
            query_id,
            &[
                Action {
                    message_id: 7,
                    kind: ActionKind::Open,
                    dwell_ms: None,
                    at: 1,
                },
                Action {
                    message_id: 7,
                    kind: ActionKind::Open,
                    dwell_ms: None,
                    at: 2,
                },
                Action {
                    message_id: 7,
                    kind: ActionKind::Archive,
                    dwell_ms: None,
                    at: 3,
                },
            ],
        )
        .await
        .expect("log actions");

    let (_, _, actions) = fx.counts();
    assert_eq!(actions, 3);
}

#[tokio::test]
async fn actions_against_an_unknown_query_are_not_found() {
    let fx = Fixture::open();
    let store = fx.store(true);

    let error = store
        .log_actions(
            424_242,
            &[Action {
                message_id: 1,
                kind: ActionKind::Open,
                dwell_ms: None,
                at: 1,
            }],
        )
        .await
        .expect_err("an unknown query_id must not silently succeed");

    assert_eq!(error.reason(), ErrorReason::NotFound);
    assert_eq!(fx.counts(), (0, 0, 0));
}

#[tokio::test]
async fn an_action_on_a_message_the_query_never_showed_is_rejected() {
    // Not hygiene — a capability bound. `LogFeedback` sits at `mail.read`
    // precisely because a caller can only talk about a page this daemon
    // already served it. Without this check a read-scoped token could attach
    // arbitrary training labels to arbitrary message ids under one of its own
    // real query ids, and task 65 would learn from them.
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");
    store
        .log_query(
            record(&fx, query_id, "q"),
            vec![impression(11, 1), impression(22, 2)],
        )
        .await
        .expect("log");

    let error = store
        .log_actions(
            query_id,
            &[Action {
                message_id: 33,
                kind: ActionKind::Open,
                dwell_ms: None,
                at: 1,
            }],
        )
        .await
        .expect_err("a message this query never showed must be rejected");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(
        error.to_string().contains("33"),
        "the rejection should name the offending message, got {error}"
    );

    // And the batch is atomic: a good action ahead of a bad one does not
    // survive. A partial batch is a *wrong* label, not a missing one — it
    // would record a result as skipped that the same request said was opened.
    let error = store
        .log_actions(
            query_id,
            &[
                Action {
                    message_id: 11,
                    kind: ActionKind::Open,
                    dwell_ms: None,
                    at: 1,
                },
                Action {
                    message_id: 33,
                    kind: ActionKind::ScrollPast,
                    dwell_ms: None,
                    at: 2,
                },
            ],
        )
        .await
        .expect_err("the whole batch must be rejected");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert_eq!(
        fx.counts(),
        (1, 2, 0),
        "the action ahead of the rejected one must have been rolled back"
    );
}

#[tokio::test]
async fn malformed_actions_are_rejected_before_anything_is_written() {
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");
    store
        .log_query(record(&fx, query_id, "q"), vec![impression(1, 1)])
        .await
        .expect("log");

    let cases: Vec<(&str, i64, Vec<Action>)> = vec![
        (
            "a dwell with no duration carries no signal",
            query_id,
            vec![Action {
                message_id: 1,
                kind: ActionKind::Dwell,
                dwell_ms: None,
                at: 1,
            }],
        ),
        (
            "a negative dwell is not a duration",
            query_id,
            vec![Action {
                message_id: 1,
                kind: ActionKind::Dwell,
                dwell_ms: Some(-1),
                at: 1,
            }],
        ),
        (
            "dwell_ms on a non-dwell action is a second definition of the same measurement",
            query_id,
            vec![Action {
                message_id: 1,
                kind: ActionKind::Open,
                dwell_ms: Some(10),
                at: 1,
            }],
        ),
        (
            "an absurd dwell would outweigh every honest signal in the corpus",
            query_id,
            vec![Action {
                message_id: 1,
                kind: ActionKind::Dwell,
                dwell_ms: Some(MAX_DWELL_MS + 1),
                at: 1,
            }],
        ),
        (
            "a pre-1970 timestamp is not a time any feedback was generated",
            query_id,
            vec![Action {
                message_id: 1,
                kind: ActionKind::Open,
                dwell_ms: None,
                at: -1,
            }],
        ),
        (
            "query_id 0 is the wire sentinel for 'not logged', not an id",
            0,
            vec![Action {
                message_id: 1,
                kind: ActionKind::Open,
                dwell_ms: None,
                at: 1,
            }],
        ),
        (
            "an unbounded batch is rejected rather than truncated",
            query_id,
            (0..=MAX_ACTIONS_PER_REQUEST)
                .map(|i| Action {
                    message_id: i as i64,
                    kind: ActionKind::Open,
                    dwell_ms: None,
                    at: 1,
                })
                .collect(),
        ),
    ];

    for (why, id, actions) in cases {
        let error = store.log_actions(id, &actions).await.expect_err(why);
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{why}");
    }

    let (_, _, written) = fx.counts();
    assert_eq!(written, 0, "a rejected batch writes nothing, not partially");
}

#[tokio::test]
async fn an_empty_action_batch_is_a_no_op() {
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");
    store
        .log_query(record(&fx, query_id, "q"), vec![impression(1, 1)])
        .await
        .expect("log");

    assert_eq!(
        store.log_actions(query_id, &[]).await.expect("no-op"),
        0,
        "a client batching zero actions is not an error"
    );
    assert_eq!(fx.counts(), (1, 1, 0));
}

// ---------------------------------------------------------------------------
// The opt-out: `search.learning = false` writes nothing at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn opting_out_writes_no_rows_at_all() {
    // The acceptance criterion, asserted on the *absence of rows* rather than
    // on any return value: a store that logged and then filtered would pass
    // an assertion about what these calls returned while leaving the user's
    // search history on disk.
    let fx = Fixture::open();
    let off = fx.store(false);

    assert!(!off.is_enabled());
    assert!(
        off.new_query_id().is_none(),
        "with learning off there is no id to stamp on a hit, so a caller \
         never builds an impression in the first place"
    );

    // Drive both write paths anyway, exactly as a caller that ignored the
    // `None` would, using an id minted out of band.
    let query_id = new_query_id();
    assert_eq!(
        off.log_query(
            record(&fx, query_id, "private search"),
            vec![impression(1, 1), impression(2, 2)],
        )
        .await
        .expect("opting out is not an error"),
        0
    );
    assert_eq!(
        off.log_actions(
            query_id,
            &[Action {
                message_id: 1,
                kind: ActionKind::Open,
                dwell_ms: None,
                at: 1,
            }],
        )
        .await
        .expect("opting out is not an error"),
        0
    );

    assert_eq!(
        fx.counts(),
        (0, 0, 0),
        "search.learning = false must leave every feedback table empty"
    );
}

#[tokio::test]
async fn turning_learning_off_stops_new_writes_against_a_populated_log() {
    // The regression that a from-empty test cannot catch: a store whose
    // opt-out only guarded the "first write" path, or that consulted config
    // once at construction and then not again on the action path.
    let fx = Fixture::open();
    let on = fx.store(true);
    let query_id = on.new_query_id().expect("learning is on");
    on.log_query(record(&fx, query_id, "q"), vec![impression(1, 1)])
        .await
        .expect("log");
    assert_eq!(fx.counts(), (1, 1, 0));

    let off = fx.store(false);
    off.log_actions(
        query_id,
        &[Action {
            message_id: 1,
            kind: ActionKind::Open,
            dwell_ms: None,
            at: 1,
        }],
    )
    .await
    .expect("opting out is not an error");
    let second = new_query_id();
    off.log_query(record(&fx, second, "also private"), vec![impression(2, 1)])
        .await
        .expect("opting out is not an error");

    assert_eq!(
        fx.counts(),
        (1, 1, 0),
        "not one new row, even with a live query to attach to"
    );
}

#[tokio::test]
async fn opting_out_skips_validation_rather_than_reporting_it() {
    // A deliberate ordering choice, pinned so it cannot drift: with learning
    // off, `log_actions` returns before it validates anything.
    //
    // Not a secrecy property — `SearchHit.query_id` is 0 for every hit when
    // learning is off, which announces the opt-out plainly, and the gRPC
    // boundary rejects a malformed enum before this method is ever reached.
    // The point is narrower and still worth pinning: with nothing going to be
    // written, there is no argument left to be invalid *about*, so a client
    // that opted out cannot be handed a rejection for a batch that was never
    // going to have an effect either way.
    let fx = Fixture::open();
    let off = fx.store(false);
    let malformed = Action {
        message_id: 1,
        kind: ActionKind::Dwell,
        dwell_ms: None,
        at: 1,
    };
    assert_eq!(
        off.log_actions(new_query_id(), &[malformed])
            .await
            .expect("no validation runs when nothing is going to be written"),
        0
    );
    assert_eq!(fx.counts(), (0, 0, 0));
}

// ---------------------------------------------------------------------------
// Exact replay: the serialized vector is the ranker's own input
// ---------------------------------------------------------------------------

#[test]
fn a_serialized_vector_replays_to_the_identical_score() {
    // The property task 65 depends on, stated as the thing that actually
    // matters rather than as a field-by-field comparison: the decoded vector
    // scores *bit for bit* the same as the original under the same intent.
    let ranker = L1Ranker::default();
    let original = rich_vector();
    let blob = encode_features(&original).expect("encode");
    let decoded = decode_features(&blob).expect("decode");

    assert_eq!(decoded, original);
    for intent in [Intent::Navigational, Intent::Exploratory, Intent::Lookup] {
        assert_eq!(
            ranker.score(&decoded, intent).to_bits(),
            ranker.score(&original, intent).to_bits(),
            "a replayed impression must reproduce the score the user was shown"
        );
    }
}

#[test]
fn the_envelope_carries_a_format_version() {
    let blob = encode_features(&rich_vector()).expect("encode");
    let json: serde_json::Value = serde_json::from_slice(&blob).expect("valid JSON");
    assert_eq!(json["version"], serde_json::json!(FEATURE_FORMAT_VERSION));
    assert!(
        json["features"].is_object(),
        "the vector is a named object, not a positional array: a feature \
         added in the middle must not reinterpret historical rows"
    );
}

#[test]
fn a_future_format_version_is_refused_rather_than_guessed_at() {
    let blob = serde_json::to_vec(&EncodedFeatures {
        version: FEATURE_FORMAT_VERSION + 1,
        features: rich_vector(),
    })
    .expect("encode");
    let error = decode_features(&blob).expect_err("a newer envelope must not be best-effort read");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn a_corrupt_blob_is_an_invalid_argument_not_a_panic() {
    let error = decode_features(b"not json at all").expect_err("garbage must not decode");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn action_and_intent_names_round_trip_through_their_stored_form() {
    // Both vocabularies are on disk, so both need a parse that agrees with
    // the writer. `ALL` makes this exhaustive by construction.
    for kind in ActionKind::ALL {
        assert_eq!(ActionKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(ActionKind::parse("hover"), None);

    for intent in [Intent::Navigational, Intent::Exploratory, Intent::Lookup] {
        assert_eq!(parse_intent(intent_name(intent)), Some(intent));
    }
    assert_eq!(parse_intent("navigation"), None);
}

#[test]
fn minted_query_ids_are_positive_and_distinct() {
    // `0` is proto3's default for `SearchHit.query_id` and therefore means
    // "this query was not logged"; a negative id would round-trip but read as
    // nonsense everywhere it surfaced.
    let ids: std::collections::HashSet<i64> = (0..10_000).map(|_| new_query_id()).collect();
    assert_eq!(ids.len(), 10_000, "minted ids collided");
    assert!(ids.iter().all(|id| *id > 0));
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// Log `count` queries, each one impression and one action, spaced a day
/// apart ending at `newest`.
async fn seed_days(fx: &Fixture, store: &FeedbackStore, count: i64, newest: i64) {
    for age in (0..count).rev() {
        let query_id = store.new_query_id().expect("learning is on");
        let mut rec = record(fx, query_id, &format!("q{age}"));
        rec.issued_at = newest - age * 24 * 60 * 60;
        store
            .log_query(rec, vec![impression(age + 1, 1)])
            .await
            .expect("log");
        store
            .log_actions(
                query_id,
                &[Action {
                    message_id: age + 1,
                    kind: ActionKind::Open,
                    dwell_ms: None,
                    at: newest - age * 24 * 60 * 60,
                }],
            )
            .await
            .expect("log actions");
    }
}

#[tokio::test]
async fn retention_drops_queries_past_the_age_horizon_and_their_children() {
    let fx = Fixture::open();
    let now = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("instant");
    let store = fx.store_with(
        true,
        FeedbackConfig {
            retention_days: 3,
            max_queries: 1_000_000,
        },
    );

    // Ten days of history, one query per day, newest issued right now.
    seed_days(&fx, &store, 10, now.timestamp()).await;
    assert_eq!(fx.counts(), (10, 10, 10));

    let pruned = store.prune_at(now).await.expect("prune");
    assert_eq!(pruned.queries, 6, "everything older than three days goes");

    let (queries, impressions, actions) = fx.counts();
    assert_eq!(queries, 4);
    assert_eq!(
        (impressions, actions),
        (4, 4),
        "impressions and actions follow their query out via ON DELETE CASCADE"
    );

    // The survivors are the newest ones, not an arbitrary three.
    let oldest: i64 = fx
        .db
        .with_read(|conn| conn.query_row("SELECT MIN(issued_at) FROM search_log", [], |r| r.get(0)))
        .expect("read oldest");
    assert!(oldest >= now.timestamp() - 3 * 24 * 60 * 60);
}

#[tokio::test]
async fn retention_drops_the_oldest_queries_past_the_count_bound() {
    let fx = Fixture::open();
    let now = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("instant");
    let store = fx.store_with(
        true,
        FeedbackConfig {
            retention_days: 36_500,
            max_queries: 4,
        },
    );

    seed_days(&fx, &store, 10, now.timestamp()).await;
    let pruned = store.prune_at(now).await.expect("prune");

    assert_eq!(pruned.queries, 6);
    assert_eq!(fx.counts(), (4, 4, 4));

    let oldest: i64 = fx
        .db
        .with_read(|conn| conn.query_row("SELECT MIN(issued_at) FROM search_log", [], |r| r.get(0)))
        .expect("read oldest");
    assert_eq!(
        oldest,
        now.timestamp() - 3 * 24 * 60 * 60,
        "the four survivors are the four newest"
    );
}

#[tokio::test]
async fn a_burst_of_queries_in_one_second_is_not_over_pruned() {
    // `issued_at` is unix *seconds*, and an interactive search box issues a
    // query per keystroke — so ties are the common case, not the edge. A cut
    // that compared against the boundary row's timestamp alone would sweep
    // every query in that second.
    let fx = Fixture::open();
    let now = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("instant");
    let store = fx.store_with(
        true,
        FeedbackConfig {
            retention_days: 36_500,
            max_queries: 5,
        },
    );

    for i in 0..10 {
        let query_id = store.new_query_id().expect("learning is on");
        let mut rec = record(&fx, query_id, &format!("keystroke {i}"));
        rec.issued_at = now.timestamp();
        store
            .log_query(rec, vec![impression(i + 1, 1)])
            .await
            .expect("log");
    }

    store.prune_at(now).await.expect("prune");
    assert_eq!(
        store.query_count().await.expect("count"),
        5,
        "exactly the count bound survives, not zero and not all ten"
    );
}

#[tokio::test]
async fn pruning_a_log_already_inside_both_bounds_removes_nothing() {
    let fx = Fixture::open();
    let now = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("instant");
    let store = fx.store_with(
        true,
        FeedbackConfig {
            retention_days: 30,
            max_queries: 100,
        },
    );

    seed_days(&fx, &store, 5, now.timestamp()).await;
    let pruned = store.prune_at(now).await.expect("prune");

    assert_eq!(pruned.queries, 0);
    assert_eq!(fx.counts(), (5, 5, 5));
}

#[tokio::test]
async fn a_zero_bound_keeps_nothing_rather_than_meaning_unlimited() {
    // A config typo that silently disabled retention would grow the log
    // without limit — the exact failure retention exists to prevent — so
    // zero has to be a real answer on both bounds.
    let fx = Fixture::open();
    let now = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("instant");

    let rows = fx.store_with(
        true,
        FeedbackConfig {
            retention_days: 36_500,
            max_queries: 0,
        },
    );
    seed_days(&fx, &rows, 3, now.timestamp()).await;
    rows.prune_at(now).await.expect("prune");
    assert_eq!(fx.counts(), (0, 0, 0));

    let days = fx.store_with(
        true,
        FeedbackConfig {
            retention_days: 0,
            max_queries: 1_000_000,
        },
    );
    seed_days(&fx, &days, 3, now.timestamp() - 60).await;
    days.prune_at(now).await.expect("prune");
    assert_eq!(fx.counts(), (0, 0, 0));
}

#[tokio::test]
async fn retention_applies_even_after_learning_is_turned_off() {
    // Opting out should *retire* what was already collected, not freeze it
    // on disk forever.
    let fx = Fixture::open();
    let now = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("instant");
    let on = fx.store_with(
        true,
        FeedbackConfig {
            retention_days: 2,
            max_queries: 1_000_000,
        },
    );
    seed_days(&fx, &on, 8, now.timestamp()).await;

    let off = fx.store_with(
        false,
        FeedbackConfig {
            retention_days: 2,
            max_queries: 1_000_000,
        },
    );
    let pruned = off.prune_at(now).await.expect("prune");
    assert_eq!(pruned.queries, 5);
    assert_eq!(fx.counts(), (3, 3, 3));
}

#[tokio::test]
async fn pruning_a_backlog_larger_than_one_chunk_converges() {
    // The chunked delete loop, driven past its own chunk size. A loop that
    // broke on the first pass would leave rows behind; one that never
    // recomputed the bound would spin.
    let fx = Fixture::open();
    let now = Utc
        .timestamp_opt(1_700_000_000, 0)
        .single()
        .expect("instant");
    let store = fx.store_with(
        true,
        FeedbackConfig {
            retention_days: 36_500,
            max_queries: 10,
        },
    );

    let total = PRUNE_CHUNK + 250;
    fx.db
        .with_write(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO search_log
                         (query_id, raw_query, norm_hash, intent, issued_at, result_count)
                     VALUES (?1, 'q', x'00', 'lookup', ?2, 0)",
                )?;
                for i in 0..total {
                    stmt.execute(rusqlite::params![i + 1, 1_600_000_000 + i])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .expect("seed backlog");

    let pruned = store.prune_at(now).await.expect("prune");
    assert_eq!(pruned.queries as i64, total - 10);
    assert_eq!(store.query_count().await.expect("count"), 10);
}

#[tokio::test]
async fn deleting_an_account_takes_its_search_history_with_it() {
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");
    store
        .log_query(record(&fx, query_id, "q"), vec![impression(1, 1)])
        .await
        .expect("log");
    store
        .log_actions(
            query_id,
            &[Action {
                message_id: 1,
                kind: ActionKind::Open,
                dwell_ms: None,
                at: 1,
            }],
        )
        .await
        .expect("log actions");
    assert_eq!(fx.counts(), (1, 1, 1));

    let account_id = fx.account_id;
    fx.db
        .with_write(move |conn| {
            conn.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
            Ok(())
        })
        .expect("delete account");

    assert_eq!(
        fx.counts(),
        (0, 0, 0),
        "deleting an account must delete the record of what was searched in it"
    );
}

#[tokio::test]
async fn an_expunged_message_does_not_take_its_training_data_with_it() {
    // The deliberate asymmetry with the account cascade above: an impression
    // is self-contained training data, and its `message_id` carries no
    // foreign key precisely so a mailbox cleanup cannot silently delete the
    // corpus. Proven by logging against a message id that never existed —
    // which is exactly what an expunged one becomes.
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");

    store
        .log_query(
            record(&fx, query_id, "q"),
            vec![impression(9_999_999, 1), impression(9_999_998, 2)],
        )
        .await
        .expect("an impression for a message that is gone is still training data");

    assert_eq!(fx.counts(), (1, 2, 0));
}

#[tokio::test]
async fn the_database_itself_refuses_a_zero_position_and_a_negative_dwell() {
    // Enforced by the schema, not only by the Rust API, so the invariant
    // holds for every write path — a future bulk import, a manual `sqlite3`
    // session, code nobody has written yet. Driven with raw SQL that never
    // touches `FeedbackStore`.
    let fx = Fixture::open();
    let store = fx.store(true);
    let query_id = store.new_query_id().expect("learning is on");
    store
        .log_query(record(&fx, query_id, "q"), vec![impression(1, 1)])
        .await
        .expect("log");

    let position = fx.db.with_write(move |conn| {
        conn.execute(
            "INSERT INTO search_impression (query_id, message_id, position, features, l1_score)
             VALUES (?1, 2, 0, x'00', 0.0)",
            [query_id],
        )?;
        Ok(())
    });
    assert!(
        position.is_err(),
        "position 0 must be rejected: ranks are 1-based"
    );

    let dwell = fx.db.with_write(move |conn| {
        conn.execute(
            "INSERT INTO search_action (query_id, message_id, action, dwell_ms, at)
             VALUES (?1, 1, 'dwell', -5, 0)",
            [query_id],
        )?;
        Ok(())
    });
    assert!(dwell.is_err(), "a negative dwell must be rejected");
}
