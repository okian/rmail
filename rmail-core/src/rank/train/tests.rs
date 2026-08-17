//! What task 65 owes: labels that correct for position bias rather than
//! relearning the incumbent ranking, a split that cannot leak, and — the
//! thing the whole task exists for — a guardrail that **refuses** a model
//! measured worse on data the trainer never saw.
//!
//! # The guardrail is tested by trying to ship a regression
//!
//! `the_guardrail_refuses_a_candidate_that_is_worse_on_the_held_out_slice`
//! is the load-bearing test here. It does not observe a good model going
//! live and infer that a bad one would not; it constructs feedback whose
//! training half points the opposite way from its held-out half, runs the
//! real `Trainer::train`, and asserts the live ranker did not move — and
//! that the refused candidate was written down so the refusal is auditable.
//! A guardrail nobody has seen refuse anything is not known to work.
//!
//! # Why so much of this is pure
//!
//! `labels` and `fit` touch no database, so the propensity model can be
//! checked as arithmetic (`a_click_at_a_deep_rank_outweighs_one_at_the_top`)
//! *and* as behaviour (`position_bias_correction_moves_the_learned_weights`)
//! without a fixture in between. Only the tests that are genuinely about
//! persistence, the swap, or rollback open a database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::fit::{fit, FitParams};
use super::labels::{
    examination_propensity, gated_values, grade, grades, is_holdout, pair_weight, pairs_for,
    LoggedQuery, ObservedAction, ShownResult, FEATURES, LONG_DWELL_MS,
};
use super::model::{decode, encode, ActiveRanker, EncodedModel, ModelError, MODEL_FORMAT_VERSION};
use super::store::ModelStatus;
use super::{TrainError, Trainer, TrainingParams};
use crate::cache::{RankerFingerprint, ResultCache};
use crate::config::{CacheConfig, IndexSemanticConfig, SearchConfig, TrainingConfig};
use crate::eval::replay::{shadow, Engagement, EngagementAction, Impression as ReplayImpression};
use crate::features::{FeatureName, FeatureVector, MatchField};
use crate::feedback::{
    encode_features, Action, ActionKind, FeedbackStore, Impression as LoggedImpression, QueryRecord,
};
use crate::query::Intent;
use crate::rank::l1::{L1Ranker, Weights};
use crate::retrieve::Source;
use crate::storage::Database;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A feature vector that contributes nothing, so a test can set exactly the
/// one or two fields it is about and know every other feature is constant
/// across the corpus (and therefore excluded from the fit — see `fit`'s
/// module docs).
fn blank() -> FeatureVector {
    FeatureVector {
        bm25_subject: 0.0,
        bm25_body: 0.0,
        bm25_from: 0.0,
        bm25_attach: 0.0,
        exact_phrase_hit: false,
        term_coverage: 0.0,
        proximity_min_span: None,
        best_match_field: MatchField::None,
        fuzzy_score: 0.0,
        cos_max_chunk: 0.0,
        cos_mean_chunk: 0.0,
        rrf_score: 0.0,
        num_sources_hit: 0,
        best_source: Source::Lexical,
        sender_affinity: 0.0,
        user_replied_thread: false,
        prior_opens_from_sender: 0.0,
        thread_activity: 0.0,
        age_days: None,
        recency_decay: 0.0,
        matches_date_intent: false,
        is_unread: false,
        is_flagged: false,
        is_pinned: false,
        ai_priority: 0.0,
        has_tag_match: false,
        folder_prior: 0.0,
        has_attachment_match: false,
        is_thread_root: false,
        thread_size: 0,
        msg_length: 0,
        sender_reputation: 0.0,
        is_newsletter: false,
        is_automated: false,
    }
}

/// `blank()` with one feature set — every synthetic corpus below is built
/// from these so exactly one axis separates a positive from a negative.
fn with_feature(name: FeatureName, value: f64) -> FeatureVector {
    let mut vector = blank();
    match name {
        FeatureName::Bm25Subject => vector.bm25_subject = value,
        FeatureName::Bm25From => vector.bm25_from = value,
        FeatureName::FuzzyScore => vector.fuzzy_score = value,
        FeatureName::CosMaxChunk => vector.cos_max_chunk = value,
        FeatureName::IsNewsletter => vector.is_newsletter = value != 0.0,
        other => unreachable!("with_feature does not know how to set {other:?}"),
    }
    vector
}

/// Behavioural equality for two weight tables.
///
/// `Weights` compares its backing map, and "absent" and "0.0" are different
/// entries there while being the same ranking function (see `l1::Weights`'
/// own "absent weight, absent contribution" convention). A decoded model
/// carries all 34 keys; a fitted one carries whatever the cold-start table
/// plus training touched. Comparing feature by feature is the comparison that
/// means "these two score identically".
fn same_weights(left: &Weights, right: &Weights) -> bool {
    FeatureName::ALL
        .into_iter()
        .all(|name| (left.get(name) - right.get(name)).abs() < f64::EPSILON)
}

fn shown(message_id: i64, position: u32, features: FeatureVector) -> ShownResult {
    ShownResult {
        message_id,
        position,
        features,
    }
}

fn acted(message_id: i64, kind: ActionKind) -> ObservedAction {
    ObservedAction {
        message_id,
        kind,
        dwell_ms: None,
    }
}

fn query(id: i64, shown: Vec<ShownResult>, actions: Vec<ObservedAction>) -> LoggedQuery {
    LoggedQuery {
        query_id: id,
        raw_query: format!("q{id}"),
        group_key: vec![0u8; 32],
        intent: Intent::Lookup,
        shown,
        actions,
    }
}

struct Fixture {
    db: Database,
    path: PathBuf,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-train-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open temp db");
        Self { db, path }
    }

    fn cache(&self) -> ResultCache {
        ResultCache::new(
            self.db.clone(),
            CacheConfig::default(),
            RankerFingerprint::new(
                &SearchConfig::default(),
                &IndexSemanticConfig::default(),
                "test-embedder",
                8,
            ),
        )
    }

    fn trainer(&self, params: TrainingParams, active: ActiveRanker) -> Trainer {
        Trainer::new(self.db.clone(), params, active, self.cache())
    }

    fn model_rows(&self) -> Vec<(i64, String, Option<i64>, f64, f64)> {
        self.db
            .with_read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, status, active, baseline_ndcg, candidate_ndcg
                     FROM ranker_model ORDER BY id",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?;
                rows.collect()
            })
            .expect("read ranker_model")
    }

    fn cached_pages(&self) -> i64 {
        self.db
            .with_read(|conn| {
                conn.query_row("SELECT count(*) FROM search_result_cache", [], |row| {
                    row.get(0)
                })
            })
            .expect("count cached pages")
    }

    fn seed_cached_page(&self) {
        self.db
            .with_write(|conn| {
                conn.execute(
                    "INSERT INTO search_result_cache
                         (cache_key, corpus_version, ranker_fingerprint, message_ids)
                     VALUES (?1, 1, ?2, ?3)",
                    rusqlite::params![vec![7u8; 32], vec![9u8; 32], vec![0u8; 8]],
                )
            })
            .expect("seed a cached page");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// Bounds small enough that a synthetic corpus of a couple of dozen queries
/// exercises the whole pipeline. The *default* bounds are asserted separately
/// (`the_default_bounds_refuse_a_cold_mailbox`) so shrinking them here cannot
/// hide a shipped default that trains on nothing.
fn training_config() -> TrainingConfig {
    TrainingConfig {
        min_queries: 4,
        min_pairs: 4,
        min_eval_queries: 3,
        holdout_percent: 25,
        epochs: 300,
        learning_rate: 0.3,
        l2: 0.0,
        ..TrainingConfig::default()
    }
}

fn test_params() -> TrainingParams {
    TrainingParams::from_config(&training_config()).expect("test params are valid")
}

/// Which side of the split a query text lands on, using the real
/// [`is_holdout`] over the real `norm_hash` — so the corpus builders below
/// place their signal by asking the splitter rather than by assuming it.
fn holdout_side(text: &str, percent: u32) -> bool {
    is_holdout(&crate::feedback::norm_hash(text), percent)
}

/// Write one logged page plus its actions through the real
/// [`FeedbackStore`], so every DB-backed test below reads exactly the rows a
/// live search would have written.
async fn log_page(
    store: &FeedbackStore,
    text: &str,
    results: &[(i64, FeatureVector)],
    actions: &[(i64, ActionKind)],
) {
    let query_id = store.new_query_id().expect("learning is on");
    let impressions = results
        .iter()
        .enumerate()
        .map(|(index, (message_id, features))| LoggedImpression {
            message_id: *message_id,
            position: u32::try_from(index + 1).expect("small page"),
            features: features.clone(),
            l1_score: 1.0 - index as f64,
            l2_score: None,
        })
        .collect();
    store
        .log_query(
            QueryRecord {
                query_id,
                account_id: None,
                raw_query: text.to_owned(),
                intent: Intent::Lookup,
                issued_at: 1_700_000_000,
            },
            impressions,
        )
        .await
        .expect("log the page");
    if actions.is_empty() {
        return;
    }
    let batch: Vec<Action> = actions
        .iter()
        .map(|(message_id, kind)| Action {
            message_id: *message_id,
            kind: *kind,
            dwell_ms: None,
            at: 1_700_000_100,
        })
        .collect();
    store
        .log_actions(query_id, &batch)
        .await
        .expect("log the actions");
}

/// The synthetic corpus every end-to-end test uses.
///
/// Two documents per query, separated on exactly one axis: `strong` carries
/// `bm25_subject = 1.0` (cold start weights it 0.90) and `weak` carries
/// `cos_max_chunk = 1.0` (0.80). The deterministic scorer therefore always
/// presents `strong` first, which is what makes "the user clicked the second
/// result" a real preference against the live ranking rather than an
/// agreement with it.
///
/// `holdout_click_strong` decides which document the *held-out* queries were
/// clicked on; the training queries always click the second one. Setting it
/// to `false` makes the trainer's conclusion agree with the holdout (the
/// candidate should win); setting it to `true` makes the holdout contradict
/// the training data, which is the regression the guardrail has to refuse.
async fn seed_corpus(fx: &Fixture, queries: usize, holdout_click_strong: bool) {
    let store = FeedbackStore::new(
        fx.db.clone(),
        true,
        crate::config::FeedbackConfig::default(),
    );
    let strong = with_feature(FeatureName::Bm25Subject, 1.0);
    let weak = with_feature(FeatureName::CosMaxChunk, 1.0);
    for n in 0..queries {
        let text = format!("corpus query {n}");
        let click_strong = holdout_side(&text, 25) && holdout_click_strong;
        let clicked = if click_strong { 10 } else { 20 };
        log_page(
            &store,
            &text,
            &[(10, strong.clone()), (20, weak.clone())],
            &[(clicked, ActionKind::Open)],
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// The flat feature form the optimizer works in
// ---------------------------------------------------------------------------

/// `fit` indexes weights and feature values by position, so the two orders
/// have to be the same one. Nothing enforces that at the type level, and a
/// feature inserted into `FeatureName::ALL` but appended to `as_pairs` would
/// silently train every weight against the wrong feature.
#[test]
fn feature_order_is_shared_by_name_and_vector() {
    assert_eq!(FEATURES, FeatureName::ALL.len());
    let pairs = blank().as_pairs();
    assert_eq!(FEATURES, pairs.len());
    for (index, (name, _)) in pairs.into_iter().enumerate() {
        assert_eq!(
            name,
            FeatureName::ALL[index],
            "as_pairs and FeatureName::ALL disagree at index {index}"
        );
    }
}

/// The trainer must fit against exactly the numbers `Weights::score`
/// multiplies its weights by, gate and all — otherwise it fits a coefficient
/// production discards.
#[test]
fn the_intent_gate_is_applied_to_the_values_the_trainer_fits() {
    let vector = with_feature(FeatureName::IsNewsletter, 1.0);
    let newsletter = FeatureName::ALL
        .iter()
        .position(|name| *name == FeatureName::IsNewsletter)
        .expect("is_newsletter is a feature");

    // Exploratory keeps the down-weight, so the value survives.
    assert!((gated_values(&vector, Intent::Exploratory)[newsletter] - 1.0).abs() < f64::EPSILON);
    // Navigational suppresses it, so the trainer sees a zero — the same zero
    // contribution `Weights::score` produces.
    assert!(gated_values(&vector, Intent::Navigational)[newsletter].abs() < f64::EPSILON);

    // And the two really do reconstruct the production score.
    let weights = Weights::cold_start();
    for intent in [Intent::Navigational, Intent::Exploratory, Intent::Lookup] {
        let gated = gated_values(&vector, intent);
        let dot: f64 = FeatureName::ALL
            .into_iter()
            .enumerate()
            .map(|(index, name)| weights.get(name) * gated[index])
            .sum();
        assert!(
            (dot - weights.score(&vector, intent)).abs() < 1e-12,
            "the flattened form does not reproduce Weights::score under {intent:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Position-bias correction
// ---------------------------------------------------------------------------

/// prd.md, verbatim: "a result clicked at position 8 is a stronger signal
/// than one clicked at position 1". This is the arithmetic half.
#[test]
fn a_click_at_a_deep_rank_outweighs_one_at_the_top() {
    let eta = 1.0;
    let ceiling = 100.0;
    let top = pair_weight(1, eta, ceiling);
    let deep = pair_weight(8, eta, ceiling);

    assert!(
        deep > top,
        "a click at rank 8 ({deep}) must outweigh one at rank 1 ({top})"
    );
    assert!((top - 1.0).abs() < 1e-12);
    assert!(
        (deep - 8.0).abs() < 1e-12,
        "1/p^-1 at p=8 should be 8, got {deep}"
    );

    // The propensity itself is the reciprocal, and monotone decreasing.
    assert!(examination_propensity(1, eta) > examination_propensity(8, eta));
    for position in 1..25u32 {
        assert!(
            pair_weight(position, eta, ceiling) <= pair_weight(position + 1, eta, ceiling),
            "weights must not decrease with depth (at position {position})"
        );
    }
}

/// Unclipped IPS has unbounded variance; the ceiling is what stops one deep
/// click from outvoting a whole corpus.
#[test]
fn a_very_deep_click_is_clipped_to_the_configured_ceiling() {
    assert!((pair_weight(50, 1.0, 10.0) - 10.0).abs() < 1e-12);
    // The floor is rank 1's own weight, so a ceiling below it cannot produce a
    // weight under 1 (or a `clamp` panic).
    assert!((pair_weight(50, 1.0, 0.5) - 1.0).abs() < 1e-12);
}

/// `eta = 0` is the documented "turn the correction off" setting; if it did
/// anything else, an operator disabling it would silently get a different
/// correction rather than none.
#[test]
fn a_zero_exponent_weights_every_position_equally() {
    for position in [1u32, 2, 9, 40] {
        assert!((pair_weight(position, 0.0, 100.0) - 1.0).abs() < 1e-12);
    }
}

/// The behavioural half of the same claim, and the one that matters: with the
/// correction on, a preference observed at a deep rank moves the learned
/// weights *more* than an otherwise identical one observed at the top. With
/// the correction off, the two move them equally — which is what pins the
/// difference on the propensity model rather than on some accident of the
/// corpus.
#[test]
fn position_bias_correction_moves_the_learned_weights() {
    // Five preferences favouring `bm25_from`, each observed at rank 1, and
    // five favouring `fuzzy_score`, each observed at rank 8. Both features
    // have a cold-start weight of 0.0, so they start level.
    let mut corpus = Vec::new();
    for n in 0..5i64 {
        corpus.push(query(
            n,
            vec![
                shown(1, 1, with_feature(FeatureName::Bm25From, 1.0)),
                shown(2, 2, blank()),
            ],
            vec![acted(1, ActionKind::Open), acted(2, ActionKind::ScrollPast)],
        ));
        corpus.push(query(
            100 + n,
            vec![
                shown(3, 8, with_feature(FeatureName::FuzzyScore, 1.0)),
                shown(4, 9, blank()),
            ],
            vec![acted(3, ActionKind::Open), acted(4, ActionKind::ScrollPast)],
        ));
    }

    let params = FitParams {
        epochs: 200,
        learning_rate: 0.2,
        l2: 0.0,
    };
    let cancel = CancellationToken::new();

    let corrected: Vec<_> = corpus
        .iter()
        .flat_map(|q| pairs_for(q, 1.0, 100.0))
        .collect();
    assert_eq!(
        corrected.len(),
        10,
        "each query should yield exactly one pair"
    );
    let corrected = fit(&corrected, &Weights::cold_start(), &params, &cancel).expect("fit");

    let deep = corrected.weights.get(FeatureName::FuzzyScore);
    let top = corrected.weights.get(FeatureName::Bm25From);
    assert!(
        deep > top,
        "the deep-rank preference should move its feature further ({deep} vs {top})"
    );

    // Same corpus, correction disabled: the asymmetry disappears entirely.
    let uncorrected: Vec<_> = corpus
        .iter()
        .flat_map(|q| pairs_for(q, 0.0, 100.0))
        .collect();
    let uncorrected = fit(&uncorrected, &Weights::cold_start(), &params, &cancel).expect("fit");
    let flat_deep = uncorrected.weights.get(FeatureName::FuzzyScore);
    let flat_top = uncorrected.weights.get(FeatureName::Bm25From);
    assert!(
        (flat_deep - flat_top).abs() < 1e-9,
        "with eta = 0 the two should move identically ({flat_deep} vs {flat_top})"
    );
}

// ---------------------------------------------------------------------------
// Which pairs exist
// ---------------------------------------------------------------------------

#[test]
fn grades_follow_the_prd_ordering() {
    assert!(grade(ActionKind::Reply, None) > grade(ActionKind::Open, None));
    assert_eq!(
        grade(ActionKind::Dwell, Some(LONG_DWELL_MS)),
        grade(ActionKind::Reply, None),
        "prd.md ranks a long dwell alongside a reply"
    );
    assert!(grade(ActionKind::Open, None) > grade(ActionKind::Dwell, Some(1_000)));
    assert!(grade(ActionKind::Dwell, Some(1_000)) > grade(ActionKind::ScrollPast, None));
    assert!(
        grade(ActionKind::Archive, None) < 0,
        "archive-from-results is prd.md's mild negative"
    );
}

#[test]
fn a_result_opened_and_then_archived_is_still_a_positive() {
    let logged = query(
        1,
        vec![shown(10, 1, blank()), shown(20, 2, blank())],
        vec![acted(10, ActionKind::Open), acted(10, ActionKind::Archive)],
    );
    let verdicts = grades(&logged);
    assert_eq!(verdicts[&10].grade, grade(ActionKind::Open, None));
    assert!(verdicts[&10].examined);
    assert_eq!(verdicts[&20].grade, 0);
    assert!(!verdicts[&20].examined);
}

/// The skip-above rule. A result the user never reached is not evidence
/// against itself, and treating it as a negative is how an offline pipeline
/// teaches itself never to surface anything new.
#[test]
fn only_results_the_user_reached_become_negatives() {
    // Clicked at rank 2: the result above it was passed over, the one below
    // was not.
    let logged = query(
        1,
        vec![
            shown(10, 1, blank()),
            shown(20, 2, with_feature(FeatureName::Bm25From, 1.0)),
            shown(30, 3, blank()),
        ],
        vec![acted(20, ActionKind::Open)],
    );
    let pairs = pairs_for(&logged, 1.0, 100.0);
    assert_eq!(
        pairs.len(),
        1,
        "only the result ranked above the click is a negative"
    );
    assert!((pairs[0].weight - 2.0).abs() < 1e-12);

    // A click at rank 1 with nothing examined below it yields nothing at all.
    let top_click = query(
        2,
        vec![shown(10, 1, blank()), shown(20, 2, blank())],
        vec![acted(10, ActionKind::Open)],
    );
    assert!(pairs_for(&top_click, 1.0, 100.0).is_empty());
}

#[test]
fn a_result_the_user_acted_on_is_a_negative_wherever_it_sat() {
    let logged = query(
        1,
        vec![shown(10, 1, blank()), shown(20, 2, blank())],
        vec![acted(10, ActionKind::Open), acted(20, ActionKind::Archive)],
    );
    let pairs = pairs_for(&logged, 1.0, 100.0);
    assert_eq!(
        pairs.len(),
        1,
        "an archived result below the click is still evidence"
    );
    assert!((pairs[0].weight - 1.0).abs() < 1e-12);
}

#[test]
fn a_page_nobody_engaged_with_yields_no_pairs() {
    let logged = query(
        1,
        vec![shown(10, 1, blank()), shown(20, 2, blank())],
        vec![],
    );
    assert!(pairs_for(&logged, 1.0, 100.0).is_empty());
}

/// An action naming a result the query never showed has no position and no
/// feature vector to attribute anything to. `feedback::repo` already refuses
/// to write one; this is what keeps a corrupt row from fabricating a label.
#[test]
fn an_action_for_an_unshown_result_is_ignored() {
    let logged = query(
        1,
        vec![shown(10, 1, blank()), shown(20, 2, blank())],
        vec![acted(999, ActionKind::Open)],
    );
    assert!(!grades(&logged).contains_key(&999));
    assert!(pairs_for(&logged, 1.0, 100.0).is_empty());
}

// ---------------------------------------------------------------------------
// The split
// ---------------------------------------------------------------------------

/// The leakage question. Every impression of the same search text shares one
/// `norm_hash`, so all of them land on the same side — otherwise the
/// guardrail would be scoring the candidate on near-duplicates of its own
/// training data and every number would look good.
#[test]
fn every_repeat_of_a_query_lands_on_one_side_of_the_split() {
    for text in ["acme invoice", "from:bob lease", "  ACME   Invoice  "] {
        let key = crate::feedback::norm_hash(text);
        let side = is_holdout(&key, 25);
        for _ in 0..10 {
            assert_eq!(
                is_holdout(&crate::feedback::norm_hash(text), 25),
                side,
                "the split moved between two evaluations of {text:?}"
            );
        }
    }
    // Normalization is what makes "the same search typed twice" one group:
    // these two differ only in spacing and case, and must not straddle the
    // line.
    assert_eq!(
        is_holdout(&crate::feedback::norm_hash("acme invoice"), 25),
        is_holdout(&crate::feedback::norm_hash("  ACME   Invoice  "), 25),
    );
}

#[test]
fn the_split_actually_divides_the_corpus() {
    let keys: Vec<Vec<u8>> = (0..400)
        .map(|n| crate::feedback::norm_hash(&format!("query number {n}")))
        .collect();
    let held = keys.iter().filter(|key| is_holdout(key, 25)).count();
    assert!(
        (60..140).contains(&held),
        "a 25% holdout over 400 groups should be near 100, got {held}"
    );
    // The degenerate settings mean what they say.
    assert!(!is_holdout(&keys[0], 0));
    assert!(is_holdout(&keys[0], 100));
}

// ---------------------------------------------------------------------------
// The optimizer
// ---------------------------------------------------------------------------

/// A feature that never varies carries no gradient, and folding a
/// near-zero sigma back into the weights would turn a rounding error into an
/// enormous coefficient. It must come out exactly where it went in.
#[test]
fn a_constant_feature_keeps_the_weight_it_started_with() {
    let corpus: Vec<LoggedQuery> = (0..5i64)
        .map(|n| {
            query(
                n,
                vec![
                    shown(1, 1, blank()),
                    shown(2, 2, with_feature(FeatureName::Bm25From, 1.0)),
                ],
                vec![acted(2, ActionKind::Open)],
            )
        })
        .collect();
    let pairs: Vec<_> = corpus
        .iter()
        .flat_map(|q| pairs_for(q, 1.0, 100.0))
        .collect();
    let start = Weights::cold_start();
    let fitted = fit(
        &pairs,
        &start,
        &FitParams {
            epochs: 50,
            learning_rate: 0.2,
            l2: 0.0,
        },
        &CancellationToken::new(),
    )
    .expect("fit");

    // `bm25_subject` is 0.0 on every document in this corpus.
    assert!(
        (fitted.weights.get(FeatureName::Bm25Subject) - start.get(FeatureName::Bm25Subject)).abs()
            < f64::EPSILON
    );
    assert!(fitted.weights.get(FeatureName::Bm25From) > start.get(FeatureName::Bm25From));
    assert!(fitted.final_loss < fitted.initial_loss);
}

/// `best_match_field` and `best_source` flatten to category ordinals, and a
/// linear model weighting an ordinal is asserting an ordering nobody claimed.
/// The cold-start table gives them no weight on exactly those grounds;
/// training must not quietly undo that on the first nightly run.
#[test]
fn training_never_learns_a_weight_for_a_category_ordinal() {
    // A corpus where the *only* thing separating the clicked result from the
    // skipped one is `best_match_field` — the strongest possible pull on a
    // feature the fit must refuse to move.
    let mut clicked = blank();
    clicked.best_match_field = MatchField::Attachment;
    let corpus: Vec<LoggedQuery> = (0..8i64)
        .map(|n| {
            query(
                n,
                vec![shown(1, 1, blank()), shown(2, 2, clicked.clone())],
                vec![acted(2, ActionKind::Open)],
            )
        })
        .collect();
    let pairs: Vec<_> = corpus
        .iter()
        .flat_map(|q| pairs_for(q, 1.0, 100.0))
        .collect();
    assert!(!pairs.is_empty(), "the corpus must produce preferences");

    let start = Weights::cold_start();
    let fitted = fit(
        &pairs,
        &start,
        &FitParams {
            epochs: 200,
            learning_rate: 0.3,
            l2: 0.0,
        },
        &CancellationToken::new(),
    )
    .expect("fit");

    for name in [FeatureName::BestMatchField, FeatureName::BestSource] {
        assert!(
            (fitted.weights.get(name) - start.get(name)).abs() < f64::EPSILON,
            "{name:?} is a category ordinal and must keep its starting weight, \
             got {}",
            fitted.weights.get(name)
        );
    }
}

#[test]
fn a_fit_with_no_pairs_returns_the_incumbent_unchanged() {
    let start = Weights::cold_start();
    let fitted = fit(
        &[],
        &start,
        &FitParams {
            epochs: 10,
            learning_rate: 0.2,
            l2: 0.0,
        },
        &CancellationToken::new(),
    )
    .expect("fit");
    assert!(same_weights(&fitted.weights, &start));
}

#[test]
fn a_cancelled_fit_stops_rather_than_returning_a_half_trained_model() {
    let corpus = [query(
        1,
        vec![
            shown(1, 1, blank()),
            shown(2, 2, with_feature(FeatureName::Bm25From, 1.0)),
        ],
        vec![acted(2, ActionKind::Open)],
    )];
    let pairs: Vec<_> = corpus
        .iter()
        .flat_map(|q| pairs_for(q, 1.0, 100.0))
        .collect();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = fit(
        &pairs,
        &Weights::cold_start(),
        &FitParams {
            epochs: 100,
            learning_rate: 0.2,
            l2: 0.0,
        },
        &cancel,
    )
    .expect_err("a cancelled fit must not produce a model");
    assert!(matches!(err, TrainError::Cancelled));
}

/// A learning rate large enough to overflow must not persist a table whose
/// every score is `NaN` — `L1Ranker::rank`'s sort answers that with silent
/// `message_id` order, which is a ranking failure nothing downstream can see.
#[test]
fn a_diverging_fit_is_refused_rather_than_persisted() {
    let corpus = [query(
        1,
        vec![
            shown(1, 1, blank()),
            shown(2, 2, with_feature(FeatureName::Bm25From, 1.0)),
        ],
        vec![acted(2, ActionKind::Open)],
    )];
    let pairs: Vec<_> = corpus
        .iter()
        .flat_map(|q| pairs_for(q, 1.0, 100.0))
        .collect();
    let err = fit(
        &pairs,
        &Weights::cold_start(),
        &FitParams {
            epochs: 10,
            learning_rate: 1e200,
            l2: 1e200,
        },
        &CancellationToken::new(),
    )
    .expect_err("a diverging fit must not produce a model");
    assert!(matches!(err, TrainError::Diverged));
}

// ---------------------------------------------------------------------------
// The stored model
// ---------------------------------------------------------------------------

#[test]
fn a_model_round_trips_through_its_envelope() {
    let mut weights = Weights::cold_start();
    weights.set(FeatureName::Bm25From, 1.25);
    weights.set(FeatureName::IsPinned, -0.5);
    let blob = encode(&weights).expect("encode");
    let decoded = decode(&blob, &Weights::cold_start()).expect("decode");
    assert!(same_weights(&decoded, &weights));

    // Every feature is written, including the zeros: a decoded model must not
    // silently inherit cold-start values for features training zeroed.
    let envelope: EncodedModel = serde_json::from_slice(&blob).expect("parse envelope");
    assert_eq!(envelope.weights.len(), FeatureName::ALL.len());
    assert_eq!(envelope.version, MODEL_FORMAT_VERSION);
}

#[test]
fn a_model_from_a_future_build_is_refused() {
    let envelope = EncodedModel {
        version: MODEL_FORMAT_VERSION + 1,
        weights: [("bm25_subject".to_owned(), 1.0)].into_iter().collect(),
    };
    let blob = serde_json::to_vec(&envelope).expect("serialize");
    assert!(matches!(
        decode(&blob, &Weights::cold_start()),
        Err(ModelError::Version { .. })
    ));
}

#[test]
fn a_model_naming_a_feature_this_build_lacks_is_refused() {
    let envelope = EncodedModel {
        version: MODEL_FORMAT_VERSION,
        weights: [("bm25_telepathy".to_owned(), 1.0)].into_iter().collect(),
    };
    let blob = serde_json::to_vec(&envelope).expect("serialize");
    assert!(matches!(
        decode(&blob, &Weights::cold_start()),
        Err(ModelError::UnknownFeature(name)) if name == "bm25_telepathy"
    ));
}

/// A diverged optimizer must not be able to persist a table whose every score
/// is `NaN`. `fit` already refuses to return one, and this is the second
/// gate: `encode` is public, and a `Weights` reaches it from Rust rather than
/// from JSON.
#[test]
fn encoding_a_non_finite_weight_is_refused() {
    let mut weights = Weights::cold_start();
    weights.set(FeatureName::Bm25From, f64::NAN);
    assert!(matches!(
        encode(&weights),
        Err(ModelError::NonFiniteWeight { .. })
    ));
}

/// The decode side needs no finiteness check of its own, and this is why: a
/// JSON number outside `f64`'s range is refused by the parser, so a corrupt
/// row surfaces as `Malformed` rather than as an infinite weight (`NaN` and
/// `inf` have no JSON literal at all). Pinned as a test because the absence
/// of that check is only safe while this stays true.
#[test]
fn a_weight_outside_the_float_range_cannot_be_decoded() {
    let blob = br#"{"version":1,"weights":{"bm25_from":1e999}}"#;
    let err = decode(blob, &Weights::cold_start()).expect_err("out of range");
    assert!(matches!(err, ModelError::Malformed(_)), "got {err:?}");
}

#[test]
fn a_malformed_blob_is_refused() {
    assert!(matches!(
        decode(b"not json at all", &Weights::cold_start()),
        Err(ModelError::Malformed(_))
    ));
}

// ---------------------------------------------------------------------------
// The live handle
// ---------------------------------------------------------------------------

/// prd.md: "Cold users fall back to the deterministic scorer." This is what
/// most mailboxes run, so it gets a test of its own rather than being assumed
/// from the absence of a model.
#[test]
fn a_cold_mailbox_scores_with_the_deterministic_scorer() {
    let active = ActiveRanker::deterministic(Weights::cold_start());
    assert_eq!(active.active_model_id(), None);

    let vector = with_feature(FeatureName::Bm25Subject, 2.0);
    let expected = L1Ranker::default().score(&vector, Intent::Lookup);
    assert!((active.current().score(&vector, Intent::Lookup) - expected).abs() < f64::EPSILON);

    // Installing and resetting move it and put it back.
    let mut learned = Weights::cold_start();
    learned.set(FeatureName::Bm25Subject, 10.0);
    active.install(7, learned);
    assert_eq!(active.active_model_id(), Some(7));
    assert!((active.current().score(&vector, Intent::Lookup) - 20.0).abs() < 1e-9);

    active.reset();
    assert_eq!(active.active_model_id(), None);
    assert!((active.current().score(&vector, Intent::Lookup) - expected).abs() < f64::EPSILON);
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[test]
fn a_configuration_that_cannot_describe_a_run_is_refused() {
    let cases = [
        TrainingConfig {
            holdout_percent: 0,
            ..TrainingConfig::default()
        },
        TrainingConfig {
            holdout_percent: 100,
            ..TrainingConfig::default()
        },
        TrainingConfig {
            position_bias_eta: -1.0,
            ..TrainingConfig::default()
        },
        TrainingConfig {
            max_propensity_weight: 0.5,
            ..TrainingConfig::default()
        },
        TrainingConfig {
            min_ndcg_gain: 2.0,
            ..TrainingConfig::default()
        },
        TrainingConfig {
            min_ndcg_gain: f64::NAN,
            ..TrainingConfig::default()
        },
        TrainingConfig {
            epochs: 0,
            ..TrainingConfig::default()
        },
        TrainingConfig {
            learning_rate: 0.0,
            ..TrainingConfig::default()
        },
        TrainingConfig {
            l2: -1.0,
            ..TrainingConfig::default()
        },
        TrainingConfig {
            min_eval_queries: 0,
            ..TrainingConfig::default()
        },
    ];
    for config in cases {
        let err = TrainingParams::from_config(&config)
            .expect_err("this configuration should not describe a training run");
        assert!(matches!(err, TrainError::InvalidConfig(_)), "got {err:?}");
    }

    // The shipped default validates, and `TrainingParams::default` — which
    // has to restate those numbers to stay total — restates them correctly.
    // Without this the fallback arm would be an untested copy that could
    // drift from the config it claims to mirror.
    let validated =
        TrainingParams::from_config(&TrainingConfig::default()).expect("the shipped default");
    assert_eq!(validated, TrainingParams::default());
}

// ---------------------------------------------------------------------------
// The evaluator this task reuses
// ---------------------------------------------------------------------------

/// `HoldoutSlice::ndcg_at_10` feeds precomputed orderings through
/// `eval::replay::shadow` as an iterator, which is only sound because
/// `shadow` calls its closure once per impression in slice order. That is
/// documented on `shadow` itself; this is the test that keeps the
/// documentation true.
#[test]
fn shadow_calls_reorder_once_per_impression_in_slice_order() {
    let impressions: Vec<ReplayImpression> = (0..5i64)
        .map(|n| ReplayImpression {
            query: format!("q{n}"),
            shown: vec![n * 10, n * 10 + 1],
            engagements: vec![Engagement {
                message_id: n * 10,
                action: EngagementAction::Open,
            }],
        })
        .collect();

    let mut seen = Vec::new();
    shadow(&impressions, |impression| {
        seen.push(impression.query.clone());
        impression.shown.clone()
    });
    assert_eq!(
        seen,
        vec![
            "q0".to_owned(),
            "q1".to_owned(),
            "q2".to_owned(),
            "q3".to_owned(),
            "q4".to_owned()
        ]
    );
}

// ---------------------------------------------------------------------------
// Refusing to train
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cold_mailbox_refuses_to_train_and_keeps_the_deterministic_scorer() {
    let fx = Fixture::open();
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active.clone());

    let err = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect_err("a mailbox with no feedback has nothing to train on");
    assert!(
        matches!(err, TrainError::InsufficientQueries { found: 0, .. }),
        "got {err:?}"
    );
    assert_eq!(active.active_model_id(), None);
    assert!(fx.model_rows().is_empty(), "a refused run writes no model");
}

/// The shipped bounds, not the deliberately small ones the end-to-end tests
/// use: a default that would train on a handful of clicks is exactly how a
/// nightly job ships a model fitted to noise.
#[tokio::test]
async fn the_default_bounds_refuse_a_small_mailbox() {
    let fx = Fixture::open();
    seed_corpus(&fx, 20, false).await;
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(TrainingParams::default(), active.clone());

    let err = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect_err("20 queries is under the shipped min_queries");
    assert!(
        matches!(
            err,
            TrainError::InsufficientQueries {
                found: 20,
                needed: 50
            }
        ),
        "got {err:?}"
    );
}

#[tokio::test]
async fn a_log_of_unclicked_searches_refuses_to_train() {
    let fx = Fixture::open();
    let store = FeedbackStore::new(
        fx.db.clone(),
        true,
        crate::config::FeedbackConfig::default(),
    );
    for n in 0..20 {
        log_page(
            &store,
            &format!("unclicked {n}"),
            &[(10, blank()), (20, blank())],
            &[],
        )
        .await;
    }
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active.clone());

    let err = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect_err("searches nobody clicked carry no preference");
    assert!(
        matches!(err, TrainError::InsufficientPairs { found: 0, .. }),
        "got {err:?}"
    );
    assert_eq!(active.active_model_id(), None);
}

/// NDCG over a slice nobody engaged with is 0.0 for every model — a tie, not
/// a comparison. Refusing beats reporting a verdict that means nothing.
#[tokio::test]
async fn a_degenerate_held_out_slice_refuses_to_judge() {
    let fx = Fixture::open();
    let store = FeedbackStore::new(
        fx.db.clone(),
        true,
        crate::config::FeedbackConfig::default(),
    );
    let strong = with_feature(FeatureName::Bm25Subject, 1.0);
    let weak = with_feature(FeatureName::CosMaxChunk, 1.0);
    for n in 0..40 {
        let text = format!("corpus query {n}");
        // Only the *training* side gets clicks; the held-out side is a page
        // nobody engaged with.
        let actions: Vec<(i64, ActionKind)> = if holdout_side(&text, 25) {
            Vec::new()
        } else {
            vec![(20, ActionKind::Open)]
        };
        log_page(
            &store,
            &text,
            &[(10, strong.clone()), (20, weak.clone())],
            &actions,
        )
        .await;
    }
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active.clone());

    let err = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect_err("a slice with no engagement cannot referee a swap");
    assert!(
        matches!(err, TrainError::DegenerateHoldout { engaged: 0, .. }),
        "got {err:?}"
    );
    assert_eq!(active.active_model_id(), None);
    assert!(fx.model_rows().is_empty());
}

// ---------------------------------------------------------------------------
// The guardrail
// ---------------------------------------------------------------------------

/// **The test this task exists for.**
///
/// The training half of the corpus says "prefer the second result"; the
/// held-out half says the opposite. The trainer therefore produces a
/// candidate that is genuinely, measurably worse on data it never saw — and
/// the live ranker must not move.
///
/// Reverting the guardrail (accepting unconditionally, or dropping the
/// `improvement > 0.0` clause and setting `min_ndcg_gain` to 0) makes this
/// test fail on the `active_model_id` assertion, which is what makes it a
/// probe rather than a restatement of the happy path.
#[tokio::test]
async fn the_guardrail_refuses_a_candidate_that_is_worse_on_the_held_out_slice() {
    let fx = Fixture::open();
    seed_corpus(&fx, 40, true).await;
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active.clone());

    let report = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect("the run itself succeeds; it is the candidate that is refused");

    assert!(
        report.candidate_ndcg_at_10 < report.baseline_ndcg_at_10,
        "the corpus is built so the candidate loses: candidate {} vs baseline {}",
        report.candidate_ndcg_at_10,
        report.baseline_ndcg_at_10
    );
    assert!(!report.accepted, "{}", report.verdict);
    assert_eq!(
        active.active_model_id(),
        None,
        "the live ranker must still be the deterministic scorer"
    );
    assert_eq!(trainer.active().active_model_id(), None);

    // The refusal is auditable: the candidate is on disk, marked rejected,
    // with the two numbers that refused it.
    let rows = fx.model_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, ModelStatus::Rejected.as_str());
    assert_eq!(rows[0].2, None, "a rejected candidate is never active");
    assert!(rows[0].4 < rows[0].3);

    // And it can never be talked into going live afterwards.
    let err = trainer
        .rollback(Some(rows[0].0))
        .await
        .expect_err("a refused candidate must not be activatable");
    assert!(matches!(err, TrainError::RefusedModel(id) if id == rows[0].0));
    assert_eq!(active.active_model_id(), None);
}

/// The guardrail's *second* clause, which the regression test above cannot
/// reach.
///
/// `improvement >= min_ndcg_gain && improvement > 0.0` — with a corpus that
/// makes the candidate worse, the first clause already refuses and the second
/// is never consulted. A quiet mailbox produces the other case: the trainer
/// converges back to the incumbent (see `fit`'s pull toward the live model),
/// both score identically on the held-out slice, and the improvement is
/// exactly `0.0`. An operator who sets `min_ndcg_gain = 0.0` would then swap
/// the model every single night for no measured benefit, rewriting the
/// history and burying the real rollback targets under ties.
///
/// The corpus here agrees with the cold-start ordering — the user clicks the
/// result the deterministic scorer already ranks first — so there is nothing
/// to learn and nothing to gain.
#[tokio::test]
async fn a_tie_is_refused_even_with_the_margin_set_to_zero() {
    let fx = Fixture::open();
    let store = FeedbackStore::new(
        fx.db.clone(),
        true,
        crate::config::FeedbackConfig::default(),
    );
    let strong = with_feature(FeatureName::Bm25Subject, 1.0);
    let weak = with_feature(FeatureName::CosMaxChunk, 1.0);
    for n in 0..40 {
        // Opened the top result and scrolled past the one below it: a real
        // preference pair, pointing exactly where the live ranker already
        // points.
        log_page(
            &store,
            &format!("corpus query {n}"),
            &[(10, strong.clone()), (20, weak.clone())],
            &[(10, ActionKind::Open), (20, ActionKind::ScrollPast)],
        )
        .await;
    }

    let params = TrainingParams::from_config(&TrainingConfig {
        min_ndcg_gain: 0.0,
        ..training_config()
    })
    .expect("a zero margin is a legal configuration");
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(params, active.clone());

    let report = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect("train");

    assert!(
        (report.candidate_ndcg_at_10 - report.baseline_ndcg_at_10).abs() < 1e-12,
        "this corpus should produce a tie, got candidate {} vs baseline {}",
        report.candidate_ndcg_at_10,
        report.baseline_ndcg_at_10
    );
    assert!((report.min_gain - 0.0).abs() < f64::EPSILON);
    assert!(
        !report.accepted,
        "a tie is not an improvement: {}",
        report.verdict
    );
    assert_eq!(
        active.active_model_id(),
        None,
        "nothing should have gone live"
    );
    let rows = fx.model_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, ModelStatus::Rejected.as_str());
}

/// NDCG over the held-out slice must be the mean over the queries that carry
/// a judgment — not over every held-out impression.
///
/// A search nobody engaged with scores `0.0` for *every* model, so averaging
/// it in multiplies both numbers, and therefore their difference, by the
/// engagement rate. At a realistic 20% that turns a configured `0.005`
/// guardrail into a real demand of `0.025`, and personalization would never
/// turn on with nothing in the report explaining why.
///
/// Here the incumbent orders every engaged held-out query perfectly, so the
/// honest baseline is `1.0`. If the silent queries were counted it would come
/// out near the engagement rate instead.
#[tokio::test]
async fn held_out_ndcg_is_not_diluted_by_searches_nobody_engaged_with() {
    let fx = Fixture::open();
    let store = FeedbackStore::new(
        fx.db.clone(),
        true,
        crate::config::FeedbackConfig::default(),
    );
    let strong = with_feature(FeatureName::Bm25Subject, 1.0);
    let weak = with_feature(FeatureName::CosMaxChunk, 1.0);
    let mut engaged_holdout = 0usize;
    let mut silent_holdout = 0usize;
    for n in 0..120 {
        let text = format!("corpus query {n}");
        let held_out = holdout_side(&text, 25);
        // Three quarters of the held-out queries are silent; the rest were
        // clicked on the result cold start already ranks first.
        let actions: Vec<(i64, ActionKind)> = if held_out {
            if n % 4 == 0 {
                engaged_holdout += 1;
                vec![(10, ActionKind::Open), (20, ActionKind::ScrollPast)]
            } else {
                silent_holdout += 1;
                Vec::new()
            }
        } else {
            vec![(10, ActionKind::Open), (20, ActionKind::ScrollPast)]
        };
        log_page(
            &store,
            &text,
            &[(10, strong.clone()), (20, weak.clone())],
            &actions,
        )
        .await;
    }
    assert!(
        engaged_holdout > 0 && silent_holdout > engaged_holdout,
        "the fixture needs a mostly-silent held-out slice: {engaged_holdout} engaged, \
         {silent_holdout} silent"
    );

    let params = TrainingParams::from_config(&TrainingConfig {
        min_eval_queries: 1,
        ..training_config()
    })
    .expect("valid");
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(params, active);

    let report = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect("train");

    assert!(
        (report.baseline_ndcg_at_10 - 1.0).abs() < 1e-12,
        "the incumbent orders every engaged held-out query perfectly, so its NDCG is 1.0; \
         a diluted number here means the silent queries were averaged in (got {})",
        report.baseline_ndcg_at_10
    );
    // And the report still shows how much of the slice was silent, so a weak
    // verdict is visible rather than hidden by the filter.
    assert!(report.holdout_queries > report.holdout_engaged);
}

/// The other side of the same guardrail: when the held-out slice agrees, the
/// swap happens, the model is on disk, and the result cache — which stores
/// *rankings* — is dropped so no page survives the model that produced it.
#[tokio::test]
async fn a_measured_win_swaps_the_model_and_drops_cached_rankings() {
    let fx = Fixture::open();
    seed_corpus(&fx, 40, false).await;
    fx.seed_cached_page();
    assert_eq!(fx.cached_pages(), 1);

    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active.clone());

    let report = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect("train");
    assert!(report.accepted, "{}", report.verdict);
    assert!(report.candidate_ndcg_at_10 > report.baseline_ndcg_at_10);
    assert!(report.candidate_ndcg_at_10 - report.baseline_ndcg_at_10 >= report.min_gain);

    let model_id = report.model_id.expect("an accepted run writes a model");
    assert_eq!(active.active_model_id(), Some(model_id));
    assert_eq!(report.active_model_id, Some(model_id));

    let rows = fx.model_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1, ModelStatus::Accepted.as_str());
    assert_eq!(rows[0].2, Some(1));

    assert_eq!(
        fx.cached_pages(),
        0,
        "a swapped model must not leave the previous model's pages cached"
    );

    // The live ranker really is the learned one: the corpus taught it to
    // prefer `cos_max_chunk` over `bm25_subject`, which the cold-start table
    // does not.
    let learned = active.current();
    assert!(
        learned.weights().get(FeatureName::CosMaxChunk)
            > learned.weights().get(FeatureName::Bm25Subject),
        "the learned model should have inverted the cold-start preference"
    );
    assert!(
        Weights::cold_start().get(FeatureName::CosMaxChunk)
            < Weights::cold_start().get(FeatureName::Bm25Subject)
    );
}

#[tokio::test]
async fn a_dry_run_measures_everything_and_changes_nothing() {
    let fx = Fixture::open();
    seed_corpus(&fx, 40, false).await;
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active.clone());

    let report = trainer
        .train(true, &CancellationToken::new())
        .await
        .expect("dry run");
    assert!(report.dry_run);
    assert!(!report.accepted, "a dry run never reports a swap");
    assert!(report.candidate_ndcg_at_10 > report.baseline_ndcg_at_10);
    assert_eq!(report.model_id, None);
    assert_eq!(active.active_model_id(), None);
    assert!(fx.model_rows().is_empty());
}

// ---------------------------------------------------------------------------
// Rollback and restore
// ---------------------------------------------------------------------------

/// prd.md: "Old model kept for rollback." Repeated rollbacks have to walk
/// *backwards* through history and terminate at the deterministic scorer.
///
/// The last two assertions are the ones with teeth. Asking for "the newest
/// accepted model" when nothing is live looks like the obvious answer and is
/// a roll *forward*: it re-activates the model the operator just stepped off,
/// and a second rollback undoes the first for ever. That is a bug this test
/// caught.
#[tokio::test]
async fn rollback_walks_back_through_history_and_then_to_the_deterministic_scorer() {
    let fx = Fixture::open();
    seed_corpus(&fx, 40, false).await;
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active.clone());

    let first_id = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect("train")
        .model_id
        .expect("an accepted run writes a model");
    assert_eq!(active.active_model_id(), Some(first_id));

    // Back to cold start, then train again: the candidate beats the
    // deterministic baseline exactly as it did the first time, so the history
    // now holds two accepted models with the newer one live.
    trainer
        .rollback(None)
        .await
        .expect("rollback to cold start");
    assert_eq!(active.active_model_id(), None);
    let second_id = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect("train again")
        .model_id
        .expect("a second accepted run writes a model");
    assert!(second_id > first_id);
    assert_eq!(active.active_model_id(), Some(second_id));

    // One step back is the *previous* accepted model, not the newest one.
    let back = trainer.rollback(None).await.expect("rollback");
    assert_eq!(back.active_model_id, Some(first_id));
    assert_eq!(active.active_model_id(), Some(first_id));

    // One more lands on the deterministic scorer...
    let bottom = trainer.rollback(None).await.expect("rollback");
    assert_eq!(bottom.active_model_id, None);
    assert_eq!(active.active_model_id(), None);

    // ...and staying there is idempotent rather than an error *and* rather
    // than a step forward into the newest accepted model.
    let again = trainer.rollback(None).await.expect("idempotent rollback");
    assert_eq!(again.active_model_id, None);
    assert_eq!(active.active_model_id(), None);
    assert!(again.detail.contains("deterministic"));

    // The stored flag agrees with what is running.
    assert!(fx.model_rows().iter().all(|row| row.2.is_none()));
    assert_eq!(fx.model_rows().len(), 2);
}

#[tokio::test]
async fn rollback_to_an_unknown_model_is_not_found() {
    let fx = Fixture::open();
    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active);
    let err = trainer
        .rollback(Some(9_999))
        .await
        .expect_err("no such model");
    assert!(
        matches!(err, TrainError::UnknownModel(9_999)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn restore_installs_the_stored_model_and_reports_what_is_live() {
    let fx = Fixture::open();
    seed_corpus(&fx, 40, false).await;
    let first = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), first.clone());
    let report = trainer
        .train(false, &CancellationToken::new())
        .await
        .expect("train");
    let model_id = report.model_id.expect("model written");

    // A fresh handle, as a restarted daemon would have.
    let restarted = ActiveRanker::deterministic(Weights::cold_start());
    let after_restart = fx.trainer(test_params(), restarted.clone());
    assert_eq!(restarted.active_model_id(), None);
    assert_eq!(
        after_restart.restore().await.expect("restore"),
        Some(model_id)
    );
    assert_eq!(restarted.active_model_id(), Some(model_id));
    assert!(
        same_weights(restarted.current().weights(), first.current().weights()),
        "the restored model must score identically to the one that was swapped in"
    );

    let history = after_restart.models(10).await.expect("models");
    assert_eq!(history.active_model_id, Some(model_id));
    assert_eq!(history.models.len(), 1);
    assert!(history.models[0].active);
    assert_eq!(history.models[0].status, ModelStatus::Accepted);
    assert!(history.models[0].train_pairs > 0);
}

/// A model this build cannot decode must leave the daemon on the
/// deterministic scorer — running, and saying so — rather than failing to
/// start or approximating a model it cannot reproduce. The row is left alone
/// so that starting the right build again picks it back up.
#[tokio::test]
async fn a_stored_model_this_build_cannot_read_falls_back_rather_than_failing() {
    let fx = Fixture::open();
    fx.db
        .with_write(|conn| {
            conn.execute(
                "INSERT INTO ranker_model (kind, weights, status, active, note)
                 VALUES ('linear', ?1, 'accepted', 1, 'from the future')",
                rusqlite::params![br#"{"version":99,"weights":{}}"#.to_vec()],
            )
        })
        .expect("seed an unreadable model");

    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active.clone());
    assert_eq!(trainer.restore().await.expect("restore"), None);
    assert_eq!(active.active_model_id(), None);

    // The row keeps its flag, and the history reports the discrepancy rather
    // than hiding it — which is what tells an operator to roll back.
    let history = trainer.models(10).await.expect("models");
    assert_eq!(history.active_model_id, None);
    assert!(history.models[0].active);
}

#[tokio::test]
async fn a_model_of_an_unrecognised_kind_is_refused() {
    let fx = Fixture::open();
    let id: i64 = fx
        .db
        .with_write(|conn| {
            // The `kind` CHECK admits only 'linear' today, so a future family
            // has to be simulated through a blob whose envelope this build
            // rejects. What is asserted here is the `by_id` path's refusal to
            // activate anything it cannot decode.
            conn.execute(
                "INSERT INTO ranker_model (kind, weights, status, note)
                 VALUES ('linear', ?1, 'accepted', 'unreadable')",
                rusqlite::params![br#"{"version":99,"weights":{}}"#.to_vec()],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .expect("seed");

    let active = ActiveRanker::deterministic(Weights::cold_start());
    let trainer = fx.trainer(test_params(), active.clone());
    let err = trainer
        .rollback(Some(id))
        .await
        .expect_err("a model this build cannot decode must not be activated");
    assert!(matches!(err, TrainError::Model(_)), "got {err:?}");
    assert_eq!(active.active_model_id(), None);
    // Critically: the flag did not move either, so the database still agrees
    // with what is running.
    assert!(fx.model_rows().iter().all(|row| row.2.is_none()));
}

/// Retention has to bound the history without destroying what it is for.
/// A guardrail doing its job refuses most candidates, so the rejected rows
/// pile up — and under one shared cap they would evict the accepted models,
/// which are the only things a rollback can land on. Rollback would then work
/// right up until the night it was needed.
#[tokio::test]
async fn pruning_bounds_the_history_without_evicting_a_rollback_target() {
    let fx = Fixture::open();
    let blob = encode(&Weights::cold_start()).expect("encode");
    let accepted_id: i64 = fx
        .db
        .with_write(|conn| {
            // One old accepted model — the rollback target — then a long run
            // of refusals on top of it, then the live model.
            conn.execute(
                "INSERT INTO ranker_model (kind, weights, status, note)
                 VALUES ('linear', ?1, 'accepted', 'the rollback target')",
                rusqlite::params![blob],
            )?;
            let accepted_id = conn.last_insert_rowid();
            for n in 0..8 {
                conn.execute(
                    "INSERT INTO ranker_model (kind, weights, status, note)
                     VALUES ('linear', ?1, 'rejected', ?2)",
                    rusqlite::params![blob, format!("refused {n}")],
                )?;
            }
            conn.execute(
                "INSERT INTO ranker_model (kind, weights, status, active, note)
                 VALUES ('linear', ?1, 'accepted', 1, 'live')",
                rusqlite::params![blob],
            )?;
            Ok(accepted_id)
        })
        .expect("seed history");

    let live_id: i64 = fx
        .db
        .with_read(|conn| {
            conn.query_row("SELECT id FROM ranker_model WHERE active = 1", [], |row| {
                row.get(0)
            })
        })
        .expect("read live id");

    fx.db
        .with_write(|conn| super::store::prune(conn, 3))
        .expect("prune");

    let rows = fx.model_rows();
    let rejected = rows
        .iter()
        .filter(|row| row.1 == ModelStatus::Rejected.as_str())
        .count();
    assert_eq!(rejected, 3, "refusals are bounded by the cap");
    assert!(
        rows.iter().any(|row| row.0 == live_id),
        "the live model is never pruned"
    );
    assert!(
        rows.iter().any(|row| row.0 == accepted_id),
        "eight refusals must not evict the model a rollback would land on"
    );
}

/// At most one model can be live, enforced by the schema rather than by Rust
/// — a half-applied swap must not be able to leave two rows claiming the
/// flag.
#[test]
fn the_schema_admits_only_one_active_model() {
    let fx = Fixture::open();
    let blob = encode(&Weights::cold_start()).expect("encode");
    let outcome = fx.db.with_write(|conn| {
        conn.execute(
            "INSERT INTO ranker_model (kind, weights, status, active) VALUES ('linear', ?1, 'accepted', 1)",
            rusqlite::params![blob],
        )?;
        conn.execute(
            "INSERT INTO ranker_model (kind, weights, status, active) VALUES ('linear', ?1, 'accepted', 1)",
            rusqlite::params![blob],
        )
    });
    assert!(
        outcome.is_err(),
        "a second active model must violate the unique index"
    );

    // And a rejected model cannot carry the flag at all.
    let rejected = fx.db.with_write(|conn| {
        conn.execute(
            "INSERT INTO ranker_model (kind, weights, status, active) VALUES ('linear', ?1, 'rejected', 1)",
            rusqlite::params![encode(&Weights::cold_start()).unwrap_or_default()],
        )
    });
    assert!(
        rejected.is_err(),
        "the schema must refuse to make a rejected candidate live"
    );
}

/// The logged page is what replay reads, so a feature vector that cannot be
/// decoded takes its whole query out of the corpus rather than leaving a page
/// with a hole in it — a hole would make the skip-above rule count documents
/// the user never passed over.
#[tokio::test]
async fn a_query_with_an_undecodable_impression_is_dropped_whole() {
    let fx = Fixture::open();
    let store = FeedbackStore::new(
        fx.db.clone(),
        true,
        crate::config::FeedbackConfig::default(),
    );
    log_page(
        &store,
        "corrupt page",
        &[(10, blank()), (20, blank())],
        &[(20, ActionKind::Open)],
    )
    .await;
    fx.db
        .with_write(|conn| {
            conn.execute(
                "UPDATE search_impression SET features = ?1 WHERE message_id = 10",
                rusqlite::params![b"not an envelope".to_vec()],
            )
        })
        .expect("corrupt one impression");

    let raw = fx
        .db
        .with_read(|conn| super::data::load(conn, 100))
        .expect("load");
    let decoded = super::data::decode(raw, &CancellationToken::new()).expect("decode");
    assert!(decoded.queries.is_empty());
    assert_eq!(decoded.skipped, 1);

    // A well-formed page beside it still survives, so the rule drops the
    // broken query rather than the corpus.
    log_page(
        &store,
        "intact page",
        &[(30, blank()), (40, blank())],
        &[(40, ActionKind::Open)],
    )
    .await;
    let raw = fx
        .db
        .with_read(|conn| super::data::load(conn, 100))
        .expect("load");
    let decoded = super::data::decode(raw, &CancellationToken::new()).expect("decode");
    assert_eq!(decoded.queries.len(), 1);
    assert_eq!(decoded.queries[0].raw_query, "intact page");
    assert_eq!(decoded.skipped, 1);
}

/// The vector the trainer reads must be byte-identical to the one the ranker
/// scored — `feedback`'s whole reason for storing it rather than
/// re-deriving it.
#[tokio::test]
async fn the_trainer_reads_back_the_exact_vector_the_ranker_scored() {
    let fx = Fixture::open();
    let store = FeedbackStore::new(
        fx.db.clone(),
        true,
        crate::config::FeedbackConfig::default(),
    );
    let vector = with_feature(FeatureName::CosMaxChunk, 0.123_456_789);
    log_page(
        &store,
        "exact replay",
        &[(10, vector.clone()), (20, blank())],
        &[(20, ActionKind::Open)],
    )
    .await;

    let raw = fx
        .db
        .with_read(|conn| super::data::load(conn, 10))
        .expect("load");
    let decoded = super::data::decode(raw, &CancellationToken::new()).expect("decode");
    assert_eq!(decoded.queries[0].shown[0].features, vector);
    assert_eq!(decoded.queries[0].shown[0].position, 1);
    assert_eq!(decoded.queries[0].intent, Intent::Lookup);
    // And the round trip really is exact, not merely close.
    assert_eq!(
        encode_features(&decoded.queries[0].shown[0].features).expect("re-encode"),
        encode_features(&vector).expect("encode"),
    );
}
