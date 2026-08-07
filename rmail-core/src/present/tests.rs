//! End-to-end proof that [`Presenter::present`] wires `mmr`/`snippet`/the
//! database together correctly — the pure per-module tests
//! (`mmr::tests`, `snippet::tests`, `batching::tests`) already pin each
//! algorithm's own correctness in isolation; this file pins the
//! integration, including the two scenarios the task's acceptance bullets
//! name explicitly: real near-identical newsletters actually diversify
//! through the real database path, and the semantic "best chunk" fallback
//! actually reads the right span.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::embed::Embedding;
use crate::query::{EntityRef, Intent, Scope, SortSpec};
use crate::repo;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-present-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open test db");
        let (account_id, mailbox_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .expect("seed account/mailbox");
        Self {
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    /// Insert a message and its `index_content` body row directly — bypassing
    /// the real extraction pipeline (task 17) so this file has byte-exact
    /// control over the body text a snippet is extracted from, the same
    /// choice `features::extract::tests`' `add_attachment_text` helper makes
    /// for the identical reason.
    async fn insert_message(&self, subject: Option<&str>, body: Option<&str>) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            subject: subject.map(str::to_owned),
            ..Default::default()
        };
        let message_id = self
            .db
            .write(move |c| repo::insert_message(c, &new))
            .await
            .expect("insert message");
        if let Some(body) = body {
            self.insert_body(message_id, body).await;
        }
        message_id
    }

    async fn insert_body(&self, message_id: i64, body: &str) {
        let body = body.to_owned();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO index_content (message_id, part, text, chars, content_hash, extractor) \
                     VALUES (?1, 'body', ?2, ?3, X'00', 'test')",
                    rusqlite::params![message_id, body, body.chars().count() as i64],
                )
            })
            .await
            .expect("insert body");
    }

    /// Insert one `chunks`/`vec_chunks` row pointing at `[span_start,
    /// span_end)` of `part`'s already-stored `index_content` text.
    async fn insert_chunk(
        &self,
        message_id: i64,
        part: &str,
        ordinal: i64,
        span_start: i64,
        span_end: i64,
        embedding: &Embedding,
    ) -> i64 {
        let part = part.to_owned();
        let bytes = embedding.to_bytes();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO chunks (message_id, part, ordinal, span_start, span_end, tokens, content_hash) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 1, X'00')",
                    rusqlite::params![message_id, part, ordinal, span_start, span_end],
                )?;
                let chunk_id = c.last_insert_rowid();
                c.execute(
                    "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
                    rusqlite::params![chunk_id, bytes],
                )?;
                Ok(chunk_id)
            })
            .await
            .expect("insert chunk")
    }

    fn presenter(&self) -> Presenter {
        Presenter::new(self.db.clone())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn no_cancel() -> CancellationToken {
    CancellationToken::new()
}

fn plan_for(raw: &str, intent: Intent) -> QueryPlan {
    QueryPlan {
        raw: raw.to_owned(),
        hard_filters: Vec::new(),
        lexical_terms: Vec::new(),
        phrases: Vec::new(),
        expansions: Vec::new(),
        query_vector: None,
        entities: Vec::<EntityRef>::new(),
        intent,
        sort: SortSpec::default(),
        scope: Scope::default(),
        needs_nl_compile: false,
    }
}

fn plain_fused(message_id: i64) -> FusedCandidate {
    FusedCandidate {
        message_id,
        fused_score: 1.0,
        hits: Vec::new(),
        num_sources_hit: 0,
        best_source: crate::retrieve::Source::Lexical,
        thread_id: None,
        thread_collapsed: Vec::new(),
        near_duplicates: Vec::new(),
    }
}

fn ranked(message_id: i64, score: f64) -> RankedCandidate {
    RankedCandidate { message_id, score }
}

// ---------------------------------------------------------------------------
// The acceptance scenario: ten newsletters, three distinct, both intents
// ---------------------------------------------------------------------------

/// A body long enough to fingerprint (`simhash::MIN_TOKENS_FOR_FINGERPRINT`
/// needs 12+ words), templated so every newsletter is close in SimHash space
/// to every other one without being byte-identical (an id/date is spliced
/// in) — the same "personalized bulk mail" shape the module docs describe.
fn newsletter_body(n: usize) -> String {
    format!(
        "weekly digest issue {n} update announcement newsletter promo offer \
         subscribe unsubscribe click here view in browser terms privacy"
    )
}

fn distinct_body(topic: &str) -> String {
    format!(
        "regarding the {topic} discussion earlier today, here is a detailed \
         and completely unrelated update with its own specific wording"
    )
}

#[tokio::test]
async fn exploratory_intent_surfaces_distinct_messages_end_to_end() {
    let fx = Fixture::open().await;
    let mut ranked_list = Vec::new();

    // Scores mirror `mmr::tests::newsletter_flood_scenario`'s own shape,
    // deliberately: the *second*-best newsletter (0.70) outscores every
    // distinct message (0.55..=0.65) by raw relevance alone, so strict
    // score order keeps it in the top 5 too. Only a genuine redundancy
    // penalty against the already-picked id 1 (real near-duplicate
    // newsletters — differing only in the spliced-in issue number — whose
    // real `simhash::fingerprint` similarity should be comfortably high)
    // can demote it below the three distinct messages. This is what makes
    // `assert_ne!` below a real proof that this method's real database +
    // real fingerprinting path is wired into `mmr::diversify`, not merely
    // that a distinct message happens to also be a top-5 result by raw
    // score regardless of whether MMR ran at all.
    let newsletter_scores = [1.0, 0.70, 0.40, 0.35, 0.30, 0.25, 0.20, 0.15, 0.10, 0.05];
    for (i, score) in newsletter_scores.into_iter().enumerate() {
        let id = fx
            .insert_message(Some("Weekly digest"), Some(&newsletter_body(i)))
            .await;
        ranked_list.push(ranked(id, score));
    }
    let mut distinct_ids = Vec::new();
    for (topic, score) in [
        ("office move", 0.65),
        ("budget review", 0.60),
        ("client escalation", 0.55),
    ] {
        let id = fx
            .insert_message(Some(topic), Some(&distinct_body(topic)))
            .await;
        distinct_ids.push(id);
        ranked_list.push(ranked(id, score));
    }

    let fused: Vec<FusedCandidate> = ranked_list
        .iter()
        .map(|c| plain_fused(c.message_id))
        .collect();
    let exploratory_plan = plan_for("update", Intent::Exploratory);
    let navigational_plan = plan_for("update", Intent::Navigational);
    let presenter = fx.presenter();

    let exploratory = presenter
        .present(
            &ranked_list,
            &fused,
            &exploratory_plan,
            mmr::DEFAULT_LAMBDA,
            5,
            &no_cancel(),
        )
        .await;
    assert_eq!(exploratory.len(), 5);
    let distinct_in_top5 = exploratory
        .iter()
        .filter(|r| distinct_ids.contains(&r.message_id))
        .count();
    assert!(
        distinct_in_top5 >= 1,
        "exploratory intent must surface at least one distinct message in the top 5: {:?}",
        exploratory.iter().map(|r| r.message_id).collect::<Vec<_>>()
    );

    let navigational = presenter
        .present(
            &ranked_list,
            &fused,
            &navigational_plan,
            mmr::DEFAULT_LAMBDA,
            5,
            &no_cancel(),
        )
        .await;
    let navigational_ids: Vec<i64> = navigational.iter().map(|r| r.message_id).collect();
    let mut sorted_by_score = ranked_list.clone();
    sorted_by_score.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.message_id.cmp(&b.message_id))
    });
    let expected_strict: Vec<i64> = sorted_by_score
        .iter()
        .take(5)
        .map(|c| c.message_id)
        .collect();
    assert_eq!(
        navigational_ids, expected_strict,
        "navigational intent must return strict score order with no diversification"
    );
    // The real proof this end-to-end test exists for: with the second
    // newsletter (id at `newsletter_scores[1]`, 0.70) genuinely outscoring
    // every distinct message by raw relevance, `expected_strict` itself
    // already contains that newsletter in the top 5 — so exploratory intent
    // producing the *same* top 5 would mean MMR's real database + real
    // fingerprinting path is not actually wired into `diversify` at all.
    // `mmr::tests::exploratory_mmr_surfaces_distinct_messages_inside_the_top_n`
    // pins the algorithm's exact composition against hand-engineered
    // fingerprints with a known Hamming distance; this assertion is the
    // complementary proof that the real path reaches it.
    let exploratory_ids: Vec<i64> = exploratory.iter().map(|r| r.message_id).collect();
    assert_ne!(
        exploratory_ids, navigational_ids,
        "exploratory intent must genuinely diversify away from strict score order, \
         not merely happen to already contain a distinct message: exploratory={exploratory_ids:?} \
         navigational={navigational_ids:?}"
    );
}

#[tokio::test]
async fn lookup_intent_also_returns_strict_score_order() {
    // See `mmr`'s own module docs: Lookup behaves like navigational here,
    // even though prd.md never names it explicitly.
    let fx = Fixture::open().await;
    let a = fx.insert_message(None, Some(&newsletter_body(0))).await;
    let b = fx.insert_message(None, Some(&newsletter_body(1))).await;
    let ranked_list = vec![ranked(a, 1.0), ranked(b, 0.9)];
    let fused: Vec<FusedCandidate> = ranked_list
        .iter()
        .map(|c| plain_fused(c.message_id))
        .collect();
    let plan = plan_for("digest", Intent::Lookup);
    let out = fx
        .presenter()
        .present(
            &ranked_list,
            &fused,
            &plan,
            mmr::DEFAULT_LAMBDA,
            2,
            &no_cancel(),
        )
        .await;
    assert_eq!(
        out.iter().map(|r| r.message_id).collect::<Vec<_>>(),
        vec![a, b]
    );
}

// ---------------------------------------------------------------------------
// Thread grouping / near-dup chip pass-through
// ---------------------------------------------------------------------------

#[tokio::test]
async fn thread_and_near_dup_metadata_pass_through_unchanged_from_fused_candidates() {
    let fx = Fixture::open().await;
    let msg = fx
        .insert_message(
            Some("Re: proposal"),
            Some("please see attached proposal document"),
        )
        .await;
    let mut fused = plain_fused(msg);
    fused.thread_id = Some(42);
    fused.thread_collapsed = vec![1001, 1002, 1003];
    fused.near_duplicates = vec![2001, 2002];

    let ranked_list = vec![ranked(msg, 1.0)];
    let plan = plan_for("proposal", Intent::Navigational);
    let out = fx
        .presenter()
        .present(
            &ranked_list,
            &[fused],
            &plan,
            mmr::DEFAULT_LAMBDA,
            10,
            &no_cancel(),
        )
        .await;

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].thread_id, Some(42));
    assert_eq!(
        out[0].thread_collapsed,
        vec![1001, 1002, 1003],
        "the +N affordance's source list"
    );
    assert_eq!(out[0].thread_collapsed.len(), 3, "the '+3 in thread' count");
    assert_eq!(out[0].near_duplicates, vec![2001, 2002]);
    assert_eq!(
        out[0].near_duplicates.len(),
        2,
        "the 'N similar' chip count"
    );
}

#[tokio::test]
async fn a_ranked_candidate_with_no_matching_fused_candidate_gets_no_thread_annotation() {
    // Should not happen in the real pipeline (Stage 4 only ever scores
    // Stage 2/3's own candidates), but must degrade rather than panic if it
    // ever does.
    let fx = Fixture::open().await;
    let msg = fx
        .insert_message(None, Some("some ordinary message body text"))
        .await;
    let ranked_list = vec![ranked(msg, 1.0)];
    let out = fx
        .presenter()
        .present(
            &ranked_list,
            &[],
            &plan_for("", Intent::Navigational),
            mmr::DEFAULT_LAMBDA,
            10,
            &no_cancel(),
        )
        .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].thread_id, None);
    assert!(out[0].thread_collapsed.is_empty());
    assert!(out[0].near_duplicates.is_empty());
}

// ---------------------------------------------------------------------------
// Lexical snippet + highlight
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_lexical_hit_gets_a_highlighted_snippet_from_its_body() {
    let fx = Fixture::open().await;
    let msg = fx
        .insert_message(
            Some("Quarterly numbers"),
            Some("please review the attached quarterly invoice before Friday"),
        )
        .await;
    let ranked_list = vec![ranked(msg, 1.0)];
    let fused = vec![plain_fused(msg)];
    let out = fx
        .presenter()
        .present(
            &ranked_list,
            &fused,
            &plan_for("invoice", Intent::Navigational),
            mmr::DEFAULT_LAMBDA,
            10,
            &no_cancel(),
        )
        .await;
    assert_eq!(out.len(), 1);
    let snip = &out[0].snippet;
    assert_eq!(snip.highlights.len(), 1);
    let highlighted = &snip.text[snip.highlights[0].clone()];
    assert_eq!(highlighted.to_lowercase(), "invoice");
}

#[tokio::test]
async fn a_message_with_no_indexed_body_gets_an_empty_snippet_not_a_panic() {
    let fx = Fixture::open().await;
    let msg = fx.insert_message(Some("No body indexed yet"), None).await;
    let ranked_list = vec![ranked(msg, 1.0)];
    let fused = vec![plain_fused(msg)];
    let out = fx
        .presenter()
        .present(
            &ranked_list,
            &fused,
            &plan_for("invoice", Intent::Navigational),
            mmr::DEFAULT_LAMBDA,
            10,
            &no_cancel(),
        )
        .await;
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].snippet, Snippet::default());
}

// ---------------------------------------------------------------------------
// Semantic "best chunk" fallback
// ---------------------------------------------------------------------------

/// A body with two paragraphs, neither containing the query's own term
/// literally — a pure semantic ("no keyword overlap") hit. Two chunks point
/// at the two paragraphs with orthogonal embeddings; the query vector
/// matches the *second* paragraph's chunk exactly. The presented snippet
/// must come from the second paragraph, not the first (which a naive
/// "always take chunk 0" or "always take the body's opening" implementation
/// would wrongly produce).
#[tokio::test]
async fn semantic_only_hit_falls_back_to_its_own_best_chunk_by_cosine_similarity() {
    let fx = Fixture::open().await;
    let first_paragraph = "greetingsalutation openingremarks pleasantries smalltalk warmup";
    let second_paragraph = "keyfindingsparagraph relevantdetails coretopic mainpoint substance";
    let body = format!("{first_paragraph} {second_paragraph}");
    let msg = fx.insert_message(Some("Notes"), Some(&body)).await;

    let first_start = 0i64;
    let first_end = first_paragraph.len() as i64;
    let second_start = first_end + 1; // +1 for the joining space
    let second_end = body.len() as i64;

    let dim = crate::index::semantic::VECTOR_DIM;
    let mut first_vec = vec![0.0f32; dim];
    first_vec[0] = 1.0;
    let mut second_vec = vec![0.0f32; dim];
    second_vec[1] = 1.0;
    let first_embedding = Embedding::new(first_vec);
    let second_embedding = Embedding::new(second_vec);

    fx.insert_chunk(msg, "body", 0, first_start, first_end, &first_embedding)
        .await;
    fx.insert_chunk(msg, "body", 1, second_start, second_end, &second_embedding)
        .await;

    let mut query_vec = vec![0.0f32; dim];
    query_vec[1] = 1.0; // matches the second paragraph's chunk exactly
    let mut plan = plan_for("nonexistentqueryterm", Intent::Navigational);
    plan.query_vector = Some(Embedding::new(query_vec));

    let ranked_list = vec![ranked(msg, 1.0)];
    let fused = vec![plain_fused(msg)];
    let out = fx
        .presenter()
        .present(
            &ranked_list,
            &fused,
            &plan,
            mmr::DEFAULT_LAMBDA,
            10,
            &no_cancel(),
        )
        .await;

    assert_eq!(out.len(), 1);
    let text = out[0].snippet.text.to_lowercase();
    assert!(
        text.contains("keyfindingsparagraph"),
        "the snippet must come from the second paragraph's chunk: {text:?}"
    );
    assert!(
        !text.contains("greetingsalutation"),
        "the snippet must not come from the first (wrong) chunk: {text:?}"
    );
}

#[tokio::test]
async fn no_query_vector_means_no_best_chunk_lookup_falls_back_to_plain_excerpt() {
    let fx = Fixture::open().await;
    let body = "opening words that do not literally contain the search phrase at all here";
    let msg = fx.insert_message(None, Some(body)).await;
    let ranked_list = vec![ranked(msg, 1.0)];
    let fused = vec![plain_fused(msg)];
    // `plan_for` leaves `query_vector: None`.
    let out = fx
        .presenter()
        .present(
            &ranked_list,
            &fused,
            &plan_for("nonexistentqueryterm", Intent::Navigational),
            mmr::DEFAULT_LAMBDA,
            10,
            &no_cancel(),
        )
        .await;
    assert_eq!(out.len(), 1);
    assert!(out[0].snippet.highlights.is_empty());
    assert!(
        !out[0].snippet.text.is_empty(),
        "a plain excerpt is still produced"
    );
}

// ---------------------------------------------------------------------------
// Robustness: empty input, cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn empty_ranked_candidates_returns_empty_with_no_query() {
    let fx = Fixture::open().await;
    let out = fx
        .presenter()
        .present(
            &[],
            &[],
            &plan_for("anything", Intent::Exploratory),
            mmr::DEFAULT_LAMBDA,
            10,
            &no_cancel(),
        )
        .await;
    assert!(out.is_empty());
}

#[tokio::test]
async fn a_limit_of_zero_returns_empty() {
    let fx = Fixture::open().await;
    let msg = fx.insert_message(None, Some("some body text here")).await;
    let ranked_list = vec![ranked(msg, 1.0)];
    let fused = vec![plain_fused(msg)];
    let out = fx
        .presenter()
        .present(
            &ranked_list,
            &fused,
            &plan_for("", Intent::Navigational),
            mmr::DEFAULT_LAMBDA,
            0,
            &no_cancel(),
        )
        .await;
    assert!(out.is_empty());
}

#[tokio::test]
async fn a_pre_cancelled_token_degrades_to_score_only_results_without_panicking() {
    let fx = Fixture::open().await;
    let msg = fx
        .insert_message(Some("Invoice"), Some("please review the attached invoice"))
        .await;
    let ranked_list = vec![ranked(msg, 1.0)];
    let fused = vec![plain_fused(msg)];
    let cancel = CancellationToken::new();
    cancel.cancel();

    let out = fx
        .presenter()
        .present(
            &ranked_list,
            &fused,
            &plan_for("invoice", Intent::Exploratory),
            mmr::DEFAULT_LAMBDA,
            10,
            &cancel,
        )
        .await;

    assert_eq!(
        out.len(),
        1,
        "a cancelled lookup still returns one entry per candidate"
    );
    assert_eq!(out[0].message_id, msg);
    assert_eq!(
        out[0].snippet,
        Snippet::default(),
        "no body was fetched, so no snippet could be built"
    );
}

// ---------------------------------------------------------------------------
// Pure helper: `strict_score_order`
// ---------------------------------------------------------------------------

#[test]
fn strict_score_order_sorts_and_truncates_even_when_given_unsorted_input() {
    let input = vec![ranked(3, 0.1), ranked(1, 0.9), ranked(2, 0.5)];
    let out = strict_score_order(&input, 2);
    assert_eq!(
        out.iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

// ---------------------------------------------------------------------------
// Pure helper: `best_chunk_per_message`
// ---------------------------------------------------------------------------

#[test]
fn best_chunk_per_message_picks_the_highest_cosine_and_ignores_invalid_spans() {
    let dim = crate::index::semantic::VECTOR_DIM;
    let mut query_vec = vec![0.0f32; dim];
    query_vec[0] = 1.0;
    let query = Embedding::new(query_vec);

    let mut matching_vec = vec![0.0f32; dim];
    matching_vec[0] = 1.0;
    let matching = Embedding::new(matching_vec).to_bytes();

    let mut orthogonal_vec = vec![0.0f32; dim];
    orthogonal_vec[1] = 1.0;
    let orthogonal = Embedding::new(orthogonal_vec).to_bytes();

    let mut part_texts = BTreeMap::new();
    part_texts.insert((1i64, "body".to_owned()), "hello world".to_owned());
    part_texts.insert((2i64, "body".to_owned()), "short".to_owned());
    part_texts.insert((3i64, "body".to_owned()), "zero width span".to_owned());

    let chunk_rows = vec![
        // Two chunks on message 1: the orthogonal one loses to the
        // matching one despite being listed first.
        (1i64, "body".to_owned(), 0i64, 5i64, orthogonal.clone()),
        (1i64, "body".to_owned(), 6i64, 11i64, matching),
        // An out-of-range span must be skipped, not panic.
        (2i64, "body".to_owned(), 0i64, 999i64, orthogonal.clone()),
        // A zero-width span (`start == end`, a corrupt or stale row) must
        // not win a slot with an empty string.
        (3i64, "body".to_owned(), 4i64, 4i64, orthogonal),
    ];
    let best = best_chunk_per_message(chunk_rows, &part_texts, &query);
    assert_eq!(best.get(&1).map(String::as_str), Some("world"));
    assert!(
        !best.contains_key(&2),
        "an out-of-range span must not produce an entry"
    );
    assert!(
        !best.contains_key(&3),
        "a zero-width span must not produce an empty entry"
    );
}

#[test]
fn best_chunk_per_message_skips_a_chunk_whose_part_text_was_never_fetched() {
    // A chunk row naming a `(message_id, part)` absent from `part_texts`
    // (a degraded/cancelled part-text fetch) must be skipped, not panic on
    // a missing lookup.
    let dim = crate::index::semantic::VECTOR_DIM;
    let mut query_vec = vec![0.0f32; dim];
    query_vec[0] = 1.0;
    let embedding = Embedding::new(query_vec.clone()).to_bytes();
    let query = Embedding::new(query_vec);
    let chunk_rows = vec![(1i64, "body".to_owned(), 0i64, 5i64, embedding)];
    let best = best_chunk_per_message(chunk_rows, &BTreeMap::new(), &query);
    assert!(best.is_empty());
}
