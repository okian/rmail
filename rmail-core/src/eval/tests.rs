//! What task 37 owes: the four metrics compute the numbers a textbook says
//! they should (checked against hand-worked examples, not against whatever
//! this implementation happens to produce), the golden set refuses every
//! shape that would make a metric meaningless, judgments resolve by RFC
//! `Message-ID` against a real corpus, replay/shadow scoring behaves
//! correctly on the labels it does and does not have, and the threshold gate
//! actually fails a regressed run.
//!
//! The half that cannot live here — that an evaluated query traverses the
//! *real* pipeline rather than a reimplementation of it — is proven in
//! `rmaild/tests/eval_service.rs`, which drives `SearchService.Evaluate`
//! over the wire against a seeded fixture corpus.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::error::ErrorReason;
use crate::eval::metrics::{ndcg_at, precision_at, recall_at, reciprocal_rank};
use crate::repo;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
    next_uid: std::cell::Cell<i64>,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-eval-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open temp db");
        let account_id = db
            .with_write(move |conn| {
                let account_id = repo::insert_account(
                    conn,
                    &repo::NewAccount {
                        name: format!("acct-{n}"),
                        ..Default::default()
                    },
                )?;
                repo::insert_mailbox(
                    conn,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok(account_id)
            })
            .expect("seed account");
        Self {
            db,
            path,
            account_id,
            next_uid: std::cell::Cell::new(1),
        }
    }

    /// A second account, for the account-scoping test.
    fn second_account(&self) -> i64 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        self.db
            .with_write(move |conn| {
                let account_id = repo::insert_account(
                    conn,
                    &repo::NewAccount {
                        name: format!("other-{n}"),
                        ..Default::default()
                    },
                )?;
                repo::insert_mailbox(
                    conn,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok(account_id)
            })
            .expect("seed second account")
    }

    fn mailbox_of(&self, account_id: i64) -> i64 {
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT id FROM mailboxes WHERE account_id = ?1 LIMIT 1",
                    [account_id],
                    |row| row.get(0),
                )
            })
            .expect("mailbox")
    }

    /// Insert a bare message carrying `rfc_id` as its `Message-ID`.
    fn insert(&self, account_id: i64, rfc_id: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let mailbox_id = self.mailbox_of(account_id);
        let new = repo::NewMessage {
            account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            message_id: Some(rfc_id.to_owned()),
            subject: Some(format!("subject for {rfc_id}")),
            ..Default::default()
        };
        self.db
            .with_write(move |conn| repo::insert_message(conn, &new))
            .expect("insert message")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// A `RankedSearch` that returns a canned answer per query string — lets the
/// `Evaluator` be tested against known-good and known-bad rankings without a
/// pipeline.
struct FakeSearch {
    answers: std::collections::HashMap<String, Vec<i64>>,
    fail: Option<String>,
}

impl FakeSearch {
    fn new() -> Self {
        Self {
            answers: std::collections::HashMap::new(),
            fail: None,
        }
    }

    fn answer(mut self, query: &str, ids: Vec<i64>) -> Self {
        self.answers.insert(query.to_owned(), ids);
        self
    }

    fn failing(query: &str) -> Self {
        Self {
            answers: std::collections::HashMap::new(),
            fail: Some(query.to_owned()),
        }
    }
}

#[async_trait::async_trait]
impl RankedSearch for FakeSearch {
    async fn ranked_ids(
        &self,
        query: &str,
        _account_id: i64,
        limit: usize,
    ) -> Result<Vec<i64>, Error> {
        if self.fail.as_deref() == Some(query) {
            return Err(Error::Internal("pipeline exploded".to_owned()));
        }
        let mut ids = self.answers.get(query).cloned().unwrap_or_default();
        ids.truncate(limit);
        Ok(ids)
    }
}

/// Judgments from `(id, gain)` pairs.
fn judged(pairs: &[(i64, u32)]) -> Judgments {
    pairs.iter().copied().collect()
}

/// `a ≈ b` to within a tolerance that is loose enough for f64 accumulation
/// and far tighter than any difference a ranking change would produce.
#[track_caller]
fn approx(a: f64, b: f64) {
    assert!(
        (a - b).abs() < 1e-9,
        "expected {b}, got {a} (difference {})",
        (a - b).abs()
    );
}

// ---------------------------------------------------------------------------
// metrics: hand-computed values
// ---------------------------------------------------------------------------

#[test]
fn ndcg_is_one_when_the_ranking_is_ideal() {
    // Gains 3, 2, 1 in descending order *is* the ideal ordering, so
    // DCG == IDCG regardless of what those numbers are.
    let j = judged(&[(1, 3), (2, 2), (3, 1)]);
    approx(ndcg_at(&[1, 2, 3], &j, 10), 1.0);
}

#[test]
fn ndcg_matches_a_hand_worked_example() {
    // One relevant document (gain 1, so 2^1-1 = 1) at rank 2:
    //   DCG  = 1 / log2(3) = 0.6309297535714574
    //   IDCG = 1 / log2(2) = 1.0
    let j = judged(&[(7, 1)]);
    approx(ndcg_at(&[9, 7, 5], &j, 10), 1.0 / 3.0f64.log2());
}

#[test]
fn ndcg_uses_exponential_gain_so_one_great_result_beats_two_mediocre_ones() {
    // A gain-3 document contributes 2^3-1 = 7; two gain-1 documents
    // contribute 1 each. Ranking the great one first must score higher than
    // ranking both mediocre ones first, which is only true under
    // exponential gain — under linear gain the second ordering would win on
    // sheer count.
    let j = judged(&[(1, 3), (2, 1), (3, 1)]);
    let great_first = ndcg_at(&[1, 2, 3], &j, 10);
    let mediocre_first = ndcg_at(&[2, 3, 1], &j, 10);
    assert!(
        great_first > mediocre_first,
        "great_first {great_first} should beat mediocre_first {mediocre_first}"
    );
}

#[test]
fn ndcg_is_clamped_to_one_when_more_relevant_docs_exist_than_the_cutoff() {
    // Twelve relevant documents but a cutoff of 10: the truncated IDCG
    // covers only the ten best, so an unclamped ratio could exceed 1.0 and
    // make "1.0 == perfect" false.
    let j: Judgments = (1..=12).map(|id| (id, 3)).collect();
    let ranked: Vec<i64> = (1..=12).collect();
    let score = ndcg_at(&ranked, &j, 10);
    approx(score, 1.0);
}

#[test]
fn reciprocal_rank_is_the_inverse_of_the_first_relevant_position() {
    let j = judged(&[(42, 1)]);
    // 42 sits third, so rank 3.
    approx(reciprocal_rank(&[1, 2, 42, 4], &j), 1.0 / 3.0);
    approx(reciprocal_rank(&[9, 8, 7, 42], &j), 0.25);
    approx(reciprocal_rank(&[42], &j), 1.0);
    // Absent entirely: 0, not an error and not a NaN.
    approx(reciprocal_rank(&[1, 2, 3], &j), 0.0);
}

#[test]
fn reciprocal_rank_has_no_cutoff_so_deep_hits_are_distinguishable_from_misses() {
    // A relevant result at rank 60 must not score the same as one that never
    // appeared — that difference is what makes a recall regression visible.
    let j = judged(&[(99, 1)]);
    let mut ranked: Vec<i64> = (1..=59).collect();
    ranked.push(99);
    let deep = reciprocal_rank(&ranked, &j);
    approx(deep, 1.0 / 60.0);
    assert!(deep > 0.0);
}

#[test]
fn recall_counts_relevant_documents_found_within_the_cutoff() {
    let j = judged(&[(1, 1), (2, 1), (3, 1), (4, 1)]);
    approx(recall_at(&[1, 2, 9, 9], &j, 50), 0.5);
    approx(recall_at(&[1, 2, 3, 4], &j, 50), 1.0);
    // Cutoff bites: only the first two positions count.
    approx(recall_at(&[1, 2, 3, 4], &j, 2), 0.5);
}

#[test]
fn precision_divides_by_the_cutoff_not_by_the_returned_count() {
    // One relevant result returned, and nothing else returned at all. P@3
    // must be 1/3 — dividing by the returned count would call this a
    // perfect 1.0 and hide the fact that two thirds of the page is missing.
    let j = judged(&[(1, 1)]);
    approx(precision_at(&[1], &j, 3), 1.0 / 3.0);
}

#[test]
fn duplicate_ids_cannot_inflate_a_score() {
    // The same relevant message emitted three times scores exactly as if it
    // were emitted once. Without the dedup, a fusion bug that repeated a hit
    // would raise NDCG instead of exposing itself.
    let j = judged(&[(1, 3), (2, 1)]);
    let honest = Metrics::score(&[1, 2], &j);
    let repeated = Metrics::score(&[1, 1, 1, 2], &j);
    approx(repeated.ndcg_at_10, honest.ndcg_at_10);
    approx(repeated.p_at_3, honest.p_at_3);
    approx(repeated.recall_at_50, honest.recall_at_50);
}

#[test]
fn degenerate_inputs_yield_zero_rather_than_nan() {
    // Every one of these would be a division by zero if written naively, and
    // a NaN that reaches a `>=` threshold check silently passes it.
    let empty = Judgments::new();
    for value in [
        ndcg_at(&[1, 2], &empty, 10),
        ndcg_at(&[], &judged(&[(1, 1)]), 10),
        ndcg_at(&[1], &judged(&[(1, 1)]), 0),
        reciprocal_rank(&[], &judged(&[(1, 1)])),
        recall_at(&[1], &empty, 50),
        recall_at(&[1], &judged(&[(1, 1)]), 0),
        precision_at(&[1], &judged(&[(1, 1)]), 0),
    ] {
        assert!(value.is_finite(), "metric produced a non-finite {value}");
        approx(value, 0.0);
    }

    // All-zero-gain judgments are "no relevant documents", not "perfect".
    approx(ndcg_at(&[1], &judged(&[(1, 0)]), 10), 0.0);
}

#[test]
fn mean_macro_averages_and_survives_an_empty_input() {
    let a = Metrics {
        ndcg_at_10: 1.0,
        mrr: 1.0,
        recall_at_50: 1.0,
        p_at_3: 1.0,
    };
    let b = Metrics::default();
    let mean = Metrics::mean(&[a, b]);
    approx(mean.ndcg_at_10, 0.5);
    approx(mean.mrr, 0.5);
    assert_eq!(Metrics::mean(&[]), Metrics::default());
}

// ---------------------------------------------------------------------------
// golden set: parsing and validation
// ---------------------------------------------------------------------------

const VALID: &str = r#"
version = 1
corpus = "fixture-v1"

[[queries]]
name = "aws-invoice"
query = "aws invoice"
judgments = [
  { message_id = "<a@example.com>", gain = 3 },
  { message_id = "<b@example.com>" },
]

[[queries]]
name = "from-operator"
query = "from:alice office move"
account_id = 7
judgments = [{ message_id = "<c@example.com>" }]
"#;

#[test]
fn a_well_formed_golden_set_parses_with_gain_defaulting_to_one() {
    let set = GoldenSet::from_toml(VALID).expect("parse");
    assert_eq!(set.version, 1);
    assert_eq!(set.corpus, "fixture-v1");
    assert_eq!(set.queries.len(), 2);

    let first = &set.queries[0];
    assert_eq!(first.name, "aws-invoice");
    assert_eq!(first.account_id, 0, "account_id defaults to all-accounts");
    assert_eq!(first.judgments[0].gain, 3);
    assert_eq!(
        first.judgments[1].gain, 1,
        "an omitted gain means plainly relevant"
    );
    assert_eq!(set.queries[1].account_id, 7);
}

#[test]
fn a_future_schema_version_is_refused_rather_than_partially_read() {
    let text = VALID.replace("version = 1", "version = 2");
    let err = GoldenSet::from_toml(&text).expect_err("version 2 must be refused");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(
        err.to_string().contains("version"),
        "error should name the version: {err}"
    );
}

#[test]
fn a_query_with_no_relevant_message_is_refused() {
    // Its NDCG would be an undefined 0/0. Better to refuse the file than to
    // average a meaningless zero into the aggregate every run.
    let text = r#"
version = 1
corpus = "c"
[[queries]]
name = "q"
query = "hello"
judgments = []
"#;
    let err = GoldenSet::from_toml(text).expect_err("must be refused");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(err.to_string().contains("no relevant message"), "{err}");
}

#[test]
fn an_explicit_zero_gain_is_refused_rather_than_read_as_irrelevant() {
    // proto3 encodes `gain = 0` and an absent `gain` identically, so a zero
    // that reached `SearchService.Evaluate` would come back as a 1 — the
    // exact inversion of what it says. Refusing it at load is what stops the
    // file's meaning and the wire's from diverging; there is no way to
    // express "judged irrelevant" here because nothing consumes one.
    let text = r#"
version = 1
corpus = "c"
[[queries]]
name = "q"
query = "hello"
judgments = [
  { message_id = "<a@example.com>", gain = 2 },
  { message_id = "<b@example.com>", gain = 0 },
]
"#;
    let err = GoldenSet::from_toml(text).expect_err("must be refused");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(err.to_string().contains("<b@example.com>"), "{err}");
    assert!(err.to_string().contains("gain 0"), "{err}");
}

#[test]
fn structurally_broken_golden_sets_are_all_refused() {
    let cases: [(&str, &str); 7] = [
        (
            "duplicate name",
            r#"version = 1
corpus = "c"
[[queries]]
name = "q"
query = "a"
judgments = [{ message_id = "<a@x>" }]
[[queries]]
name = "q"
query = "b"
judgments = [{ message_id = "<b@x>" }]"#,
        ),
        (
            "empty name",
            r#"version = 1
corpus = "c"
[[queries]]
name = "   "
query = "a"
judgments = [{ message_id = "<a@x>" }]"#,
        ),
        (
            "empty query string",
            r#"version = 1
corpus = "c"
[[queries]]
name = "q"
query = ""
judgments = [{ message_id = "<a@x>" }]"#,
        ),
        (
            "no queries",
            r#"version = 1
corpus = "c"
queries = []"#,
        ),
        (
            "empty corpus",
            r#"version = 1
corpus = "  "
[[queries]]
name = "q"
query = "a"
judgments = [{ message_id = "<a@x>" }]"#,
        ),
        (
            "gain over the maximum",
            r#"version = 1
corpus = "c"
[[queries]]
name = "q"
query = "a"
judgments = [{ message_id = "<a@x>", gain = 9 }]"#,
        ),
        (
            "same message judged twice",
            r#"version = 1
corpus = "c"
[[queries]]
name = "q"
query = "a"
judgments = [{ message_id = "<a@x>" }, { message_id = "<a@x>", gain = 2 }]"#,
        ),
    ];

    for (label, text) in cases {
        let outcome = GoldenSet::from_toml(text);
        assert!(
            outcome.is_err(),
            "{label}: should have been refused, but parsed"
        );
        let err = outcome.expect_err("checked is_err above");
        assert_eq!(
            err.reason(),
            ErrorReason::InvalidArgument,
            "{label}: wrong reason ({err})"
        );
    }
}

#[test]
fn malformed_toml_reports_invalid_argument_not_a_panic() {
    let err = GoldenSet::from_toml("this is not toml {{{").expect_err("must fail");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn a_missing_golden_set_file_is_not_found() {
    let missing = std::env::temp_dir().join("rmail-eval-definitely-absent.toml");
    let err = GoldenSet::load(&missing).expect_err("must fail");
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

// ---------------------------------------------------------------------------
// golden set: resolution against a real corpus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn judgments_resolve_by_rfc_message_id() {
    let f = Fixture::open();
    let a = f.insert(f.account_id, "<a@example.com>");
    let b = f.insert(f.account_id, "<b@example.com>");

    let q = GoldenQuery {
        name: "q".to_owned(),
        query: "anything".to_owned(),
        account_id: 0,
        judgments: vec![
            JudgedMessage {
                message_id: "<a@example.com>".to_owned(),
                gain: 3,
            },
            JudgedMessage {
                message_id: "<b@example.com>".to_owned(),
                gain: 1,
            },
        ],
    };

    let resolved = q.resolve(&f.db).await.expect("resolve");
    assert!(resolved.unresolved.is_empty());
    assert_eq!(resolved.judgments.get(&a), Some(&3));
    assert_eq!(resolved.judgments.get(&b), Some(&1));
}

#[tokio::test]
async fn an_unknown_message_id_is_reported_not_silently_dropped() {
    // The distinction this test defends: a fixture that failed to seed must
    // not be indistinguishable from a ranker that got worse.
    let f = Fixture::open();
    f.insert(f.account_id, "<a@example.com>");

    let q = GoldenQuery {
        name: "q".to_owned(),
        query: "anything".to_owned(),
        account_id: 0,
        judgments: vec![
            JudgedMessage {
                message_id: "<a@example.com>".to_owned(),
                gain: 1,
            },
            JudgedMessage {
                message_id: "<nowhere@example.com>".to_owned(),
                gain: 1,
            },
        ],
    };

    let resolved = q.resolve(&f.db).await.expect("resolve");
    assert_eq!(resolved.judgments.len(), 1);
    assert_eq!(
        resolved.unresolved,
        vec!["<nowhere@example.com>".to_owned()]
    );
}

#[tokio::test]
async fn one_message_id_in_two_accounts_resolves_to_both_copies() {
    // The same mail delivered twice is the same answer either way, so both
    // rows carry the judged gain and whichever the pipeline surfaces counts.
    let f = Fixture::open();
    let other = f.second_account();
    let mine = f.insert(f.account_id, "<dup@example.com>");
    let theirs = f.insert(other, "<dup@example.com>");

    let q = GoldenQuery {
        name: "q".to_owned(),
        query: "anything".to_owned(),
        account_id: 0,
        judgments: vec![JudgedMessage {
            message_id: "<dup@example.com>".to_owned(),
            gain: 2,
        }],
    };

    let resolved = q.resolve(&f.db).await.expect("resolve");
    assert_eq!(resolved.judgments.get(&mine), Some(&2));
    assert_eq!(resolved.judgments.get(&theirs), Some(&2));
}

#[tokio::test]
async fn an_account_scoped_query_does_not_resolve_through_another_account() {
    // A judgment must not be satisfied by a copy sitting in an account the
    // query itself excluded — that would credit the ranker for a result it
    // could never have returned.
    let f = Fixture::open();
    let other = f.second_account();
    f.insert(other, "<elsewhere@example.com>");

    let q = GoldenQuery {
        name: "q".to_owned(),
        query: "anything".to_owned(),
        account_id: f.account_id,
        judgments: vec![JudgedMessage {
            message_id: "<elsewhere@example.com>".to_owned(),
            gain: 1,
        }],
    };

    let resolved = q.resolve(&f.db).await.expect("resolve");
    assert!(resolved.judgments.is_empty());
    assert_eq!(
        resolved.unresolved,
        vec!["<elsewhere@example.com>".to_owned()]
    );
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

/// A golden set over two seeded messages, plus the fixture that holds them.
async fn two_query_set(f: &Fixture) -> (GoldenSet, i64, i64) {
    let a = f.insert(f.account_id, "<a@example.com>");
    let b = f.insert(f.account_id, "<b@example.com>");
    let set = GoldenSet {
        version: 1,
        corpus: "fixture".to_owned(),
        queries: vec![
            GoldenQuery {
                name: "finds-a".to_owned(),
                query: "alpha".to_owned(),
                account_id: 0,
                judgments: vec![JudgedMessage {
                    message_id: "<a@example.com>".to_owned(),
                    gain: 1,
                }],
            },
            GoldenQuery {
                name: "finds-b".to_owned(),
                query: "beta".to_owned(),
                account_id: 0,
                judgments: vec![JudgedMessage {
                    message_id: "<b@example.com>".to_owned(),
                    gain: 1,
                }],
            },
        ],
    };
    (set, a, b)
}

#[tokio::test]
async fn a_perfect_ranker_scores_one_across_the_board() {
    let f = Fixture::open();
    let (set, a, b) = two_query_set(&f).await;
    let search = FakeSearch::new()
        .answer("alpha", vec![a])
        .answer("beta", vec![b]);

    let report = Evaluator::new(f.db.clone())
        .run(&set, &search)
        .await
        .expect("run");

    assert_eq!(report.corpus, "fixture");
    assert_eq!(report.per_query.len(), 2);
    approx(report.aggregate.ndcg_at_10, 1.0);
    approx(report.aggregate.mrr, 1.0);
    approx(report.aggregate.recall_at_50, 1.0);
    // P@3 with one relevant document out of three slots is 1/3 even for a
    // perfect ranker — the metric's denominator is the cutoff.
    approx(report.aggregate.p_at_3, 1.0 / 3.0);
    assert!(report.unresolved().is_empty());
}

#[tokio::test]
async fn a_ranker_that_returns_nothing_scores_zero_and_is_attributable() {
    let f = Fixture::open();
    let (set, a, _b) = two_query_set(&f).await;
    // "alpha" is answered perfectly; "beta" returns nothing.
    let search = FakeSearch::new().answer("alpha", vec![a]);

    let report = Evaluator::new(f.db.clone())
        .run(&set, &search)
        .await
        .expect("run");

    approx(report.aggregate.ndcg_at_10, 0.5);
    let worst = report.worst(1);
    assert_eq!(
        worst[0].name, "finds-b",
        "the worst query must be identifiable, not just the aggregate"
    );
    approx(worst[0].metrics.ndcg_at_10, 0.0);
    assert_eq!(worst[0].returned, 0);
}

#[tokio::test]
async fn a_pipeline_error_fails_the_run_rather_than_scoring_zero() {
    // Averaging a zero in would report a broken build as a relevance
    // regression, sending whoever reads the CI log after the wrong bug.
    let f = Fixture::open();
    let (set, _a, _b) = two_query_set(&f).await;
    let search = FakeSearch::failing("alpha");

    let err = Evaluator::new(f.db.clone())
        .run(&set, &search)
        .await
        .expect_err("must propagate");
    assert_eq!(err.reason(), ErrorReason::Internal);
}

#[tokio::test]
async fn the_evaluator_asks_for_at_least_fifty_results() {
    // Recall@50 over a 25-result page is unmeasurable, not merely low, so a
    // caller asking for less is overridden.
    let f = Fixture::open();
    let (set, a, b) = two_query_set(&f).await;

    #[derive(Default)]
    struct LimitSpy(std::sync::Mutex<Vec<usize>>);
    #[async_trait::async_trait]
    impl RankedSearch for LimitSpy {
        async fn ranked_ids(
            &self,
            _query: &str,
            _account_id: i64,
            limit: usize,
        ) -> Result<Vec<i64>, Error> {
            #[allow(clippy::unwrap_used)]
            self.0.lock().unwrap().push(limit);
            Ok(Vec::new())
        }
    }

    let spy = LimitSpy::default();
    let _ = (a, b);
    let _ = Evaluator::new(f.db.clone())
        .with_limit(5)
        .run(&set, &spy)
        .await
        .expect("run");

    #[allow(clippy::unwrap_used)]
    let seen = spy.0.lock().unwrap().clone();
    assert!(
        seen.iter().all(|l| *l >= RECALL_K),
        "limits should be clamped up to {RECALL_K}, saw {seen:?}"
    );
}

// ---------------------------------------------------------------------------
// Thresholds — the regression guard itself
// ---------------------------------------------------------------------------

fn report_scoring(ndcg: f64) -> EvalReport {
    EvalReport {
        corpus: "fixture".to_owned(),
        per_query: vec![QueryEval {
            name: "q".to_owned(),
            query: "q".to_owned(),
            metrics: Metrics {
                ndcg_at_10: ndcg,
                mrr: ndcg,
                recall_at_50: ndcg,
                p_at_3: ndcg,
            },
            returned: 1,
            relevant: 1,
            unresolved: Vec::new(),
        }],
        aggregate: Metrics {
            ndcg_at_10: ndcg,
            mrr: ndcg,
            recall_at_50: ndcg,
            p_at_3: ndcg,
        },
    }
}

#[test]
fn the_gate_passes_at_the_threshold_and_fails_below_it() {
    let thresholds = EvalThresholds {
        min_ndcg_at_10: 0.8,
        ..EvalThresholds::default()
    };
    thresholds
        .check(&report_scoring(0.8))
        .expect("exactly at the threshold must pass");
    let err = thresholds
        .check(&report_scoring(0.7999))
        .expect_err("below the threshold must fail");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    assert!(err.to_string().contains("NDCG@10"), "{err}");
}

#[test]
fn the_gate_reports_every_violated_threshold_at_once() {
    let thresholds = EvalThresholds {
        min_ndcg_at_10: 0.9,
        min_mrr: Some(0.9),
        min_recall_at_50: Some(0.9),
        min_p_at_3: Some(0.9),
        require_resolved: true,
    };
    let err = thresholds
        .check(&report_scoring(0.1))
        .expect_err("must fail");
    let text = err.to_string();
    for metric in ["NDCG@10", "MRR", "Recall@50", "P@3"] {
        assert!(text.contains(metric), "{metric} missing from: {text}");
    }
}

#[test]
fn an_unresolved_judgment_fails_the_gate_even_when_the_metrics_pass() {
    // The whole point: a corpus that did not seed must not be reported as a
    // healthy run just because the queries that *did* resolve scored well.
    let mut report = report_scoring(1.0);
    report.per_query[0].unresolved = vec!["<missing@example.com>".to_owned()];

    let thresholds = EvalThresholds {
        min_ndcg_at_10: 0.5,
        ..EvalThresholds::default()
    };
    let err = thresholds.check(&report).expect_err("must fail");
    assert!(err.to_string().contains("<missing@example.com>"), "{err}");

    // ...and is waivable for the developer-mailbox case where a partially
    // synced corpus is expected.
    EvalThresholds {
        require_resolved: false,
        ..thresholds
    }
    .check(&report)
    .expect("waived");
}

#[test]
fn the_default_threshold_is_not_a_no_op() {
    // A default of 0.0 would be a gate that exists only on paper.
    assert!(EvalThresholds::default().min_ndcg_at_10 > 0.0);
    assert!(EvalThresholds::default().require_resolved);
}

// ---------------------------------------------------------------------------
// replay & shadow
// ---------------------------------------------------------------------------

fn impression(query: &str, shown: &[i64], engagements: &[(i64, EngagementAction)]) -> Impression {
    Impression {
        query: query.to_owned(),
        shown: shown.to_vec(),
        engagements: engagements
            .iter()
            .map(|(message_id, action)| Engagement {
                message_id: *message_id,
                action: *action,
            })
            .collect(),
    }
}

#[test]
fn only_wanting_a_result_counts_as_positive_engagement() {
    assert!(EngagementAction::Open.is_positive());
    assert!(EngagementAction::Reply.is_positive());
    assert!(EngagementAction::Dwell.is_positive());
    // Archiving from a result list is disposal, not success.
    assert!(!EngagementAction::Archive.is_positive());
    assert!(!EngagementAction::ScrollPast.is_positive());
}

#[test]
fn replay_computes_the_online_metrics_over_logged_pages() {
    let log = [
        // Engaged at rank 1.
        impression("a", &[1, 2, 3], &[(1, EngagementAction::Open)]),
        // Engaged at rank 3.
        impression("b", &[4, 5, 6], &[(6, EngagementAction::Reply)]),
        // Abandoned.
        impression("c", &[7, 8, 9], &[(7, EngagementAction::ScrollPast)]),
        // Engaged at rank 4 — inside CTR, outside success@3.
        impression("d", &[1, 2, 3, 4], &[(4, EngagementAction::Dwell)]),
    ];

    let m = replay(&log);
    assert_eq!(m.impressions, 4);
    approx(m.ctr, 0.75);
    approx(m.abandonment, 0.25);
    approx(m.success_at_1, 0.25);
    approx(m.success_at_3, 0.5);
    // (1/1 + 1/3 + 0 + 1/4) / 4
    approx(m.engaged_mrr, (1.0 + 1.0 / 3.0 + 0.25) / 4.0);
}

#[test]
fn an_engagement_on_a_message_that_was_never_shown_is_ignored() {
    // A malformed log entry must not inject a judgment for a document no
    // ranker could have returned.
    let log = [impression("a", &[1, 2], &[(99, EngagementAction::Open)])];
    assert!(!log[0].is_successful());
    approx(replay(&log).ctr, 0.0);
}

#[test]
fn replay_of_an_empty_log_is_all_zeros_not_a_division_by_zero() {
    let m = replay(&[]);
    assert_eq!(m, OnlineMetrics::default());
    assert!(m.engaged_mrr.is_finite());
}

#[test]
fn shadow_scoring_rewards_a_candidate_that_promotes_the_engaged_result() {
    // The incumbent buried the engaged result at rank 3; the candidate puts
    // it first. That must show up as a win on both metric families.
    let log = [impression("q", &[1, 2, 3], &[(3, EngagementAction::Open)])];

    let baseline = replay(&log);
    let candidate = shadow(&log, |imp| {
        let mut order = imp.shown.clone();
        order.reverse();
        order
    });

    approx(baseline.engaged_mrr, 1.0 / 3.0);
    approx(candidate.online.engaged_mrr, 1.0);
    approx(candidate.online.success_at_1, 1.0);
    assert!(candidate.ranking.ndcg_at_10 > 0.0);
    assert_eq!(candidate.unlabeled, 0);
}

#[test]
fn shadow_scoring_drops_unlabeled_candidates_and_says_how_many() {
    // A candidate surfacing documents the log never displayed has no labels
    // for them. Counting them as "not engaged" would punish exactly the
    // behaviour a new ranker is adopted for, so they are dropped and
    // reported instead.
    let log = [impression("q", &[1, 2], &[(1, EngagementAction::Open)])];

    let outcome = shadow(&log, |_| vec![50, 51, 1, 52]);
    assert_eq!(outcome.unlabeled, 3);
    // Of the labeled documents, the engaged one came first.
    approx(outcome.online.engaged_mrr, 1.0);
    approx(outcome.online.success_at_1, 1.0);
}

#[test]
fn a_candidate_that_omits_the_engaged_result_loses_recall() {
    let log = [impression("q", &[1, 2, 3], &[(3, EngagementAction::Open)])];
    let outcome = shadow(&log, |_| vec![1, 2]);
    approx(outcome.online.engaged_mrr, 0.0);
    approx(outcome.ranking.recall_at_50, 0.0);
    // The impression is still counted — it was abandoned under this ranker,
    // not skipped.
    assert_eq!(outcome.online.impressions, 1);
}

#[test]
fn bucketing_is_deterministic_stable_and_spread() {
    // Same query, same bucket — every time, or a user sees one arm and then
    // the other for the same query and the experiment is contaminated.
    for q in ["invoice", "from:alice", ""] {
        let first = bucket(q, 2);
        for _ in 0..100 {
            assert_eq!(bucket(q, 2), first);
        }
    }

    // "No experiment" is everyone in the control arm, not a panic.
    assert_eq!(bucket("anything", 0), 0);
    assert_eq!(bucket("anything", 1), 0);

    // In range, and actually using both arms over a realistic query mix.
    let queries: Vec<String> = (0..200).map(|i| format!("query number {i}")).collect();
    let mut counts = [0usize; 4];
    for q in &queries {
        let b = bucket(q, 4) as usize;
        assert!(b < 4);
        counts[b] += 1;
    }
    assert!(
        counts.iter().all(|c| *c > 10),
        "buckets should be roughly balanced, got {counts:?}"
    );
}
