//! What task 31/33/64/65 need proven about [`FeatureExtractor`] beyond "it
//! compiles": determinism given a fixed `now` (task 65's replay depends on
//! it), that every feature group actually computes a real value from the
//! local database rather than a hardcoded default (this task's "vector
//! completeness" acceptance bullet), that `bm25_*` and `term_coverage`
//! genuinely diverge the way the module docs claim, and that a missing
//! message, a cancelled lookup, or a degenerate dense-hit score all degrade
//! rather than panic.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::Bm25Weights;
use crate::fuse::{FusedCandidate, SourceHit};
use crate::index::fts::FtsIndex;
use crate::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use crate::query::{DateRange, Filter, HardFilter, Intent, Operator, QueryPlan, Scope, SortSpec};
use crate::repo;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-features-extract-{pid}-{n}.db"));
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
        let fts = FtsIndex::new(db.clone(), Bm25Weights::default());
        Self {
            queue: IndexQueue::new(db.clone(), QueueOptions::default()),
            fts,
            db,
            account_id,
            mailbox_id,
            next_uid: Cell::new(1),
            path,
        }
    }

    async fn mailbox_named(&self, name: &str) -> i64 {
        let account_id = self.account_id;
        let name = name.to_owned();
        self.db
            .write(move |c| {
                repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name,
                        ..Default::default()
                    },
                )
            })
            .await
            .expect("insert mailbox")
    }

    async fn insert_thread(&self) -> i64 {
        let account_id = self.account_id;
        self.db
            .write(move |c| {
                repo::insert_thread(
                    c,
                    &repo::NewThread {
                        account_id,
                        ..Default::default()
                    },
                )
            })
            .await
            .expect("insert thread")
    }

    async fn set_thread_stats(
        &self,
        thread_id: i64,
        message_count: i64,
        last_message_at: Option<i64>,
    ) {
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE threads SET message_count = ?1, last_message_at = ?2 WHERE id = ?3",
                    rusqlite::params![message_count, last_message_at, thread_id],
                )
            })
            .await
            .expect("update thread stats");
    }

    async fn set_thread_root(&self, thread_id: i64, root_message_id: i64) {
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE threads SET root_message_id = ?1 WHERE id = ?2",
                    rusqlite::params![root_message_id, thread_id],
                )
            })
            .await
            .expect("update thread root");
    }

    /// Insert a message, extract it, and index it — the real pipeline (same
    /// as `retrieve::lexical::tests::Fixture::index`), so `bm25_*`/
    /// `best_match_field`/`has_attachment_match` exercise the genuine FTS5
    /// path rather than a hand-built fixture of it.
    async fn index(&self, mailbox_id: Option<i64>, new: repo::NewMessage) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: mailbox_id.unwrap_or(self.mailbox_id),
            uid,
            uidvalidity: 1,
            ..new
        };
        let message_id = self
            .db
            .write(move |c| repo::insert_message(c, &new))
            .await
            .expect("insert message");
        extract_message(&self.db, &self.queue, message_id, PRIORITY_NORMAL)
            .await
            .expect("extract message");
        self.fts
            .index_message(message_id)
            .await
            .expect("index message");
        message_id
    }

    /// Attach text to an attachment part and re-fold the FTS row — the
    /// `attachments` column is what `bm25_attach`/`has_attachment_match`
    /// read.
    async fn add_attachment_text(&self, message_id: i64, part_id: &str, text: &str) {
        let part = format!("attachment:{part_id}");
        let text = text.to_owned();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO index_content (message_id, part, text, chars, content_hash, extractor) \
                     VALUES (?1, ?2, ?3, ?4, X'00', 'test')",
                    rusqlite::params![message_id, part, text, text.len() as i64],
                )
            })
            .await
            .expect("insert attachment text");
        self.fts
            .index_message(message_id)
            .await
            .expect("re-index message");
    }

    async fn flag(&self, message_id: i64, flag: &str) {
        let flag = flag.to_owned();
        self.db
            .write(move |c| repo::add_flag(c, message_id, &flag))
            .await
            .expect("add flag");
    }

    async fn seed_contact(&self, address: &str, message_count: i64, last_seen: Option<i64>) {
        let address = address.to_owned();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO contacts (address, name, message_count, last_seen) \
                     VALUES (?1, NULL, ?2, ?3)",
                    rusqlite::params![address, message_count, last_seen],
                )
            })
            .await
            .expect("seed contact");
    }

    fn extractor(&self) -> FeatureExtractor {
        FeatureExtractor::new(self.db.clone(), Bm25Weights::default(), 30.0)
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

fn plan_for(raw: &str) -> QueryPlan {
    QueryPlan {
        raw: raw.to_owned(),
        hard_filters: Vec::new(),
        lexical_terms: Vec::new(),
        phrases: Vec::new(),
        expansions: Vec::new(),
        query_vector: None,
        entities: Vec::new(),
        intent: Intent::Navigational,
        sort: SortSpec::default(),
        scope: Scope::default(),
        needs_nl_compile: false,
    }
}

fn plain_candidate(message_id: i64) -> FusedCandidate {
    FusedCandidate {
        message_id,
        fused_score: 1.0,
        hits: vec![SourceHit {
            source: Source::Lexical,
            rank: 1,
            score: 1.0,
            mean_score: None,
        }],
        num_sources_hit: 1,
        best_source: Source::Lexical,
        thread_id: None,
        thread_collapsed: Vec::new(),
        near_duplicates: Vec::new(),
    }
}

/// The one anchor instant every test that cares about "now" uses, so a
/// message's `date` can be expressed as an offset from it rather than a
/// magic timestamp.
fn anchor_now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0)
        .single()
        .expect("valid anchor")
}

fn days_ago(now: chrono::DateTime<Utc>, days: i64) -> i64 {
    (now - ChronoDuration::days(days)).timestamp()
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[tokio::test]
async fn extract_at_is_deterministic_given_the_same_now() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("Invoice from Acme".to_owned()),
                body_text: Some("please find the invoice attached".to_owned()),
                from_addr: Some("billing@acme.com".to_owned()),
                date: Some(days_ago(now, 5)),
                ..Default::default()
            },
        )
        .await;
    fx.flag(msg, "\\Flagged").await;
    let candidates = vec![plain_candidate(msg)];
    let plan = plan_for("invoice");
    let extractor = fx.extractor();

    let first = extractor
        .extract_at(&candidates, &plan, now, &no_cancel())
        .await;
    let second = extractor
        .extract_at(&candidates, &plan, now, &no_cancel())
        .await;

    assert_eq!(first, second);
    let json_a = serde_json::to_string(&first).expect("serialize");
    let json_b = serde_json::to_string(&second).expect("serialize");
    assert_eq!(json_a, json_b, "byte-identical replay");
}

/// The strongest proof `now` is a parameter, not a hidden clock read: two
/// calls separated by real wall-clock time, given the identical injected
/// `now`, must still produce byte-identical output — a bug that secretly
/// called `Utc::now()` internally would make this test flaky/fail depending
/// on when it happened to run.
#[tokio::test]
async fn extract_at_ignores_real_wall_clock_time() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("Weekly digest".to_owned()),
                date: Some(days_ago(now, 2)),
                ..Default::default()
            },
        )
        .await;
    let candidates = vec![plain_candidate(msg)];
    let plan = plan_for("digest");
    let extractor = fx.extractor();

    let before = extractor
        .extract_at(&candidates, &plan, now, &no_cancel())
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    let after = extractor
        .extract_at(&candidates, &plan, now, &no_cancel())
        .await;

    assert_eq!(
        before, after,
        "real time passing must not change a fixed-`now` extraction"
    );
}

/// Changing the injected `now` changes `age_days`/`recency_decay`
/// predictably — the complementary half of the determinism contract: `now`
/// actually drives the computation, it is not simply ignored.
#[tokio::test]
async fn now_parameter_drives_age_and_recency_decay() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                date: Some(days_ago(now, 10)),
                ..Default::default()
            },
        )
        .await;
    let candidates = vec![plain_candidate(msg)];
    let plan = plan_for("");
    let extractor = fx.extractor();

    let at_t0 = extractor
        .extract_at(&candidates, &plan, now, &no_cancel())
        .await;
    let at_t0_plus_10 = extractor
        .extract_at(
            &candidates,
            &plan,
            now + ChronoDuration::days(10),
            &no_cancel(),
        )
        .await;

    let age0 = at_t0[0].features.age_days.expect("date is known");
    let age10 = at_t0_plus_10[0].features.age_days.expect("date is known");
    assert!((age0 - 10.0).abs() < 1e-6, "age0 = {age0}");
    assert!((age10 - 20.0).abs() < 1e-6, "age10 = {age10}");
    assert!(
        at_t0_plus_10[0].features.recency_decay < at_t0[0].features.recency_decay,
        "an older message must decay to a lower recency score"
    );
}

// ---------------------------------------------------------------------------
// Textual group: bm25 isolation, term_coverage, exact_phrase_hit, proximity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bm25_fields_are_isolated_per_column_and_best_match_field_follows() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("Invoice ready".to_owned()),
                body_text: Some("unrelated shipping notice".to_owned()),
                from_addr: Some("alice@example.com".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let candidates = vec![plain_candidate(msg)];
    let plan = plan_for("invoice");
    let out = fx
        .extractor()
        .extract_at(&candidates, &plan, anchor_now(), &no_cancel())
        .await;
    let f = &out[0].features;

    assert!(f.bm25_subject > 0.0, "the term is in the subject");
    assert_eq!(f.bm25_body, 0.0, "the term never appears in the body");
    assert_eq!(f.bm25_from, 0.0);
    assert_eq!(f.bm25_attach, 0.0);
    assert_eq!(f.best_match_field, MatchField::Subject);
    assert!(!f.has_attachment_match);
}

#[tokio::test]
async fn bm25_attach_and_has_attachment_match_come_from_attachment_text() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("FYI".to_owned()),
                body_text: Some("see the file".to_owned()),
                ..Default::default()
            },
        )
        .await;
    fx.add_attachment_text(msg, "p1", "confidential merger agreement")
        .await;
    let candidates = vec![plain_candidate(msg)];
    let plan = plan_for("confidential");
    let out = fx
        .extractor()
        .extract_at(&candidates, &plan, anchor_now(), &no_cancel())
        .await;
    let f = &out[0].features;

    assert!(f.bm25_attach > 0.0);
    assert_eq!(f.bm25_subject, 0.0);
    assert_eq!(f.bm25_body, 0.0);
    assert_eq!(f.best_match_field, MatchField::Attachment);
    assert!(f.has_attachment_match);
}

/// A positive check for `bm25_from` and `bm25_body` specifically —
/// `bm25_fields_are_isolated_per_column_and_best_match_field_follows` above
/// only ever proves they are `0.0` on a subject-only match, which a mis-map
/// of `index::fts::COLUMNS`' column indices (`isolated_weights`' `subject=0,
/// from=1, ..., body=3, attachments=4`) would pass just as easily. prd.md's
/// Stage 4 cold-start formula weights `bm25_body` directly
/// (`+ 0.35 * bm25_body`), so a silently-broken body column is not a
/// cosmetic bug.
#[tokio::test]
async fn bm25_from_and_bm25_body_are_positive_when_the_term_is_there() {
    let fx = Fixture::open().await;
    let from_only = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("nothing relevant".to_owned()),
                body_text: Some("nothing relevant either".to_owned()),
                from_addr: Some("kickoff@example.com".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let body_only = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("nothing relevant".to_owned()),
                body_text: Some("the kickoff meeting is tomorrow".to_owned()),
                from_addr: Some("nobody@example.com".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let plan = plan_for("kickoff");
    let extractor = fx.extractor();

    let from_out = extractor
        .extract_at(
            &[plain_candidate(from_only)],
            &plan,
            anchor_now(),
            &no_cancel(),
        )
        .await;
    let body_out = extractor
        .extract_at(
            &[plain_candidate(body_only)],
            &plan,
            anchor_now(),
            &no_cancel(),
        )
        .await;

    assert!(
        from_out[0].features.bm25_from > 0.0,
        "term is only in the sender address"
    );
    assert_eq!(from_out[0].features.bm25_body, 0.0);
    assert_eq!(from_out[0].features.best_match_field, MatchField::From);

    assert!(
        body_out[0].features.bm25_body > 0.0,
        "term is only in the body"
    );
    assert_eq!(body_out[0].features.bm25_from, 0.0);
    assert_eq!(body_out[0].features.best_match_field, MatchField::Body);
}

/// A pure unit test of [`sanitize_bm25_row`] with values SQLite's own
/// `bm25()` would never actually produce (`NaN`, `±inf`) — the real FTS5
/// path can only ever be exercised with values SQLite itself computed,
/// which are always finite, so this is the only way to prove
/// `best_match_field`/`has_attachment_match` (both of which read a
/// [`Bm25Fields`] field with `> 0.0`) can never desync from a stray
/// non-finite value that bypassed sanitization.
#[test]
fn sanitize_bm25_row_never_produces_a_non_finite_field() {
    let fields = sanitize_bm25_row(f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.0);
    assert!(fields.subject.is_finite());
    assert!(fields.from.is_finite());
    assert!(fields.body.is_finite());
    assert!(fields.attach.is_finite());
    assert_eq!(
        best_match_field(&fields),
        MatchField::None,
        "no positive signal survives sanitization to report"
    );
}

/// `term_coverage` credits a candidate for the query terms it actually
/// contains even when the strict `AND`-required `bm25_*` fields cannot
/// (because one query term is entirely absent) — the divergence the module
/// docs describe as deliberate.
#[tokio::test]
async fn term_coverage_diverges_from_bm25_on_partial_overlap() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("alpha project kickoff".to_owned()),
                body_text: Some("no relation to the other word".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let candidates = vec![plain_candidate(msg)];
    let plan = plan_for("alpha beta");
    let out = fx
        .extractor()
        .extract_at(&candidates, &plan, anchor_now(), &no_cancel())
        .await;
    let f = &out[0].features;

    assert!(
        (f.term_coverage - 0.5).abs() < 1e-9,
        "one of two terms present"
    );
    assert_eq!(
        f.bm25_subject, 0.0,
        "the AND-required match must fail: 'beta' never appears anywhere"
    );
}

/// `term_coverage`'s local scan only sees [`MAX_BODY_CHARS_FOR_SCAN`] of
/// body text, cheap by design — but a real body past that cap with every
/// query term present would otherwise report `bm25_body > 0.0` (FTS5
/// indexed the *whole* body) while a capped local scan claimed the terms
/// were missing, directly contradicting a stronger signal in the very same
/// vector. When the strict `AND` match succeeds (`bm25_matched`), that
/// authoritative full-text result must win over the truncated local scan.
#[tokio::test]
async fn term_coverage_trusts_a_confirmed_bm25_match_past_the_scan_cap() {
    let fx = Fixture::open().await;
    let padding = "filler ".repeat((MAX_BODY_CHARS_FOR_SCAN as usize / "filler ".len()) + 50);
    let body = format!("{padding}kickoff meeting details at the very end of a long body");
    assert!(
        body.len() > MAX_BODY_CHARS_FOR_SCAN as usize + 200,
        "the body must genuinely exceed the scan cap for this test to mean anything"
    );
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                body_text: Some(body),
                ..Default::default()
            },
        )
        .await;
    let plan = plan_for("kickoff meeting");
    let out = fx
        .extractor()
        .extract_at(&[plain_candidate(msg)], &plan, anchor_now(), &no_cancel())
        .await;
    let f = &out[0].features;

    assert!(
        f.bm25_body > 0.0,
        "FTS5 indexed the whole body, past the scan cap"
    );
    assert_eq!(
        f.term_coverage, 1.0,
        "term_coverage must not contradict a confirmed full-text match"
    );
}

#[tokio::test]
async fn term_coverage_is_vacuously_full_with_no_free_text_terms() {
    let fx = Fixture::open().await;
    let msg = fx.index(None, repo::NewMessage::default()).await;
    let candidates = vec![plain_candidate(msg)];
    let plan = plan_for("from:alice"); // operator only, no free-text terms
    let out = fx
        .extractor()
        .extract_at(&candidates, &plan, anchor_now(), &no_cancel())
        .await;
    assert_eq!(out[0].features.term_coverage, 1.0);
}

/// A repeated query term (`"invoice invoice enhancement"`) must count once
/// in `term_coverage`'s denominator, not twice — otherwise a query that
/// happens to repeat a word scores differently than the same word typed
/// once, and disagrees with [`proximity_min_span`]'s own distinct-term
/// handling of the identical input.
#[tokio::test]
async fn term_coverage_deduplicates_repeated_query_terms() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("invoice reminder".to_owned()),
                body_text: Some("nothing else relevant here".to_owned()),
                ..Default::default()
            },
        )
        .await;
    // "enhancement" never appears, so the strict `AND` fails and this
    // exercises the real per-term local scan, not the `bm25_matched`
    // short-circuit — the only path `scan_terms`'s dedup actually affects.
    let plan = plan_for("invoice invoice enhancement");
    let out = fx
        .extractor()
        .extract_at(&[plain_candidate(msg)], &plan, anchor_now(), &no_cancel())
        .await;
    assert_eq!(
        out[0].features.bm25_subject, 0.0,
        "the AND-required match must fail: 'enhancement' never appears"
    );
    assert_eq!(
        out[0].features.term_coverage, 0.5,
        "one of two *distinct* terms present, not one of three raw tokens"
    );
}

/// `scan_terms` excludes `~`-forced-semantic terms, matching
/// `build_required_match`'s own filter — this is what keeps the
/// `bm25_matched` short-circuit sound (see `term_coverage_trusts_a_confirmed_bm25_match_past_the_scan_cap`).
/// This test exercises the *other* half: a query that is entirely
/// `~`-forced-semantic has nothing left for `build_required_match` to
/// require, so `bm25_matched` is always `false` here and the real per-term
/// local scan runs — proving the semantic term itself is excluded from
/// `term_coverage`'s denominator, not merely irrelevant because the
/// short-circuit happened to cover for it.
#[tokio::test]
async fn term_coverage_excludes_forced_semantic_terms_from_the_local_scan_too() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("totally unrelated content".to_owned()),
                body_text: Some("nothing about that other word".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let plan = plan_for("~missingword");
    let out = fx
        .extractor()
        .extract_at(&[plain_candidate(msg)], &plan, anchor_now(), &no_cancel())
        .await;
    assert_eq!(
        out[0].features.bm25_subject, 0.0,
        "a `~`-forced term contributes nothing to the required AND"
    );
    assert_eq!(
        out[0].features.term_coverage, 1.0,
        "a `~`-forced-semantic term absent from the text must not count as an \
         uncovered free-text term"
    );
}

#[tokio::test]
async fn exact_phrase_hit_requires_verbatim_adjacency() {
    let fx = Fixture::open().await;
    let verbatim = fx
        .index(
            None,
            repo::NewMessage {
                body_text: Some("the quarterly report is attached".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let scrambled = fx
        .index(
            None,
            repo::NewMessage {
                body_text: Some("report: quarterly figures inside".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let plan = plan_for("\"quarterly report\"");
    let extractor = fx.extractor();

    let hit = extractor
        .extract_at(
            &[plain_candidate(verbatim)],
            &plan,
            anchor_now(),
            &no_cancel(),
        )
        .await;
    let miss = extractor
        .extract_at(
            &[plain_candidate(scrambled)],
            &plan,
            anchor_now(),
            &no_cancel(),
        )
        .await;

    assert!(hit[0].features.exact_phrase_hit);
    assert!(!miss[0].features.exact_phrase_hit);
}

#[tokio::test]
async fn proximity_min_span_prefers_the_tighter_window_and_is_none_when_a_term_is_missing() {
    let fx = Fixture::open().await;
    let tight = fx
        .index(
            None,
            repo::NewMessage {
                body_text: Some("alpha beta somewhere else entirely".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let loose = fx
        .index(
            None,
            repo::NewMessage {
                body_text: Some("alpha noise noise noise noise beta".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let missing = fx
        .index(
            None,
            repo::NewMessage {
                body_text: Some("alpha only, no second term here".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let plan = plan_for("alpha beta");
    let extractor = fx.extractor();

    let tight_out = extractor
        .extract_at(&[plain_candidate(tight)], &plan, anchor_now(), &no_cancel())
        .await;
    let loose_out = extractor
        .extract_at(&[plain_candidate(loose)], &plan, anchor_now(), &no_cancel())
        .await;
    let missing_out = extractor
        .extract_at(
            &[plain_candidate(missing)],
            &plan,
            anchor_now(),
            &no_cancel(),
        )
        .await;

    let tight_span = tight_out[0]
        .features
        .proximity_min_span
        .expect("both terms present");
    let loose_span = loose_out[0]
        .features
        .proximity_min_span
        .expect("both terms present");
    assert!(
        tight_span < loose_span,
        "tight={tight_span} loose={loose_span}"
    );
    assert_eq!(missing_out[0].features.proximity_min_span, None);
}

// ---------------------------------------------------------------------------
// Status group: flags, and the four "no table yet" features
// ---------------------------------------------------------------------------

#[tokio::test]
async fn is_unread_and_is_flagged_read_from_the_flags_table() {
    let fx = Fixture::open().await;
    let unread_and_flagged = fx.index(None, repo::NewMessage::default()).await;
    fx.flag(unread_and_flagged, "\\Flagged").await;
    let read = fx.index(None, repo::NewMessage::default()).await;
    fx.flag(read, "\\Seen").await;

    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(
            &[plain_candidate(unread_and_flagged), plain_candidate(read)],
            &plan,
            anchor_now(),
            &no_cancel(),
        )
        .await;

    assert!(out[0].features.is_unread);
    assert!(out[0].features.is_flagged);
    assert!(!out[1].features.is_unread);
    assert!(!out[1].features.is_flagged);
}

/// prd.md names `is_pinned`/`has_tag_match`/`ai_priority`/
/// `prior_opens_from_sender` but this build has no backing table for any of
/// them — see the module docs' "No table yet" section. All four must be a
/// real, reachable `false`/`0.0`, not merely "whatever the default happens
/// to be."
#[tokio::test]
async fn features_with_no_backing_table_default_honestly() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("anything".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let plan = plan_for("anything");
    let out = fx
        .extractor()
        .extract_at(&[plain_candidate(msg)], &plan, anchor_now(), &no_cancel())
        .await;
    let f = &out[0].features;
    assert!(!f.is_pinned);
    assert!(!f.has_tag_match);
    assert_eq!(f.ai_priority, 0.0);
    assert_eq!(f.prior_opens_from_sender, 0.0);
}

#[tokio::test]
async fn folder_prior_ranks_inbox_above_archive_above_spam() {
    let fx = Fixture::open().await;
    let archive_id = fx.mailbox_named("Archive").await;
    let spam_id = fx.mailbox_named("Spam").await;
    let inbox_msg = fx.index(None, repo::NewMessage::default()).await; // fixture's own INBOX
    let archive_msg = fx
        .index(Some(archive_id), repo::NewMessage::default())
        .await;
    let spam_msg = fx.index(Some(spam_id), repo::NewMessage::default()).await;

    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(
            &[
                plain_candidate(inbox_msg),
                plain_candidate(archive_msg),
                plain_candidate(spam_msg),
            ],
            &plan,
            anchor_now(),
            &no_cancel(),
        )
        .await;

    assert!(
        out[0].features.folder_prior > out[1].features.folder_prior,
        "inbox > archive"
    );
    assert!(
        out[1].features.folder_prior > out[2].features.folder_prior,
        "archive > spam"
    );
}

// ---------------------------------------------------------------------------
// Structural / personal group: threads
// ---------------------------------------------------------------------------

#[tokio::test]
async fn thread_root_size_and_activity() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    let thread = fx.insert_thread().await;
    let root = fx
        .index(
            None,
            repo::NewMessage {
                thread_id: Some(thread),
                date: Some(days_ago(now, 3)),
                ..Default::default()
            },
        )
        .await;
    let reply = fx
        .index(
            None,
            repo::NewMessage {
                thread_id: Some(thread),
                date: Some(days_ago(now, 1)),
                ..Default::default()
            },
        )
        .await;
    fx.set_thread_root(thread, root).await;
    fx.set_thread_stats(thread, 3, Some(days_ago(now, 1))).await;

    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(
            &[plain_candidate(root), plain_candidate(reply)],
            &plan,
            now,
            &no_cancel(),
        )
        .await;

    assert!(out[0].features.is_thread_root);
    assert!(!out[1].features.is_thread_root);
    assert_eq!(out[0].features.thread_size, 3);
    assert_eq!(out[1].features.thread_size, 3);
    assert!(
        out[0].features.thread_activity > 0.0,
        "a recently-active thread scores positive"
    );
}

/// `\Answered` on *any* message in the thread marks every candidate from
/// that thread as `user_replied_thread` — this is a thread-level signal, not
/// a per-message one.
#[tokio::test]
async fn user_replied_thread_is_thread_wide() {
    let fx = Fixture::open().await;
    let thread = fx.insert_thread().await;
    let first = fx
        .index(
            None,
            repo::NewMessage {
                thread_id: Some(thread),
                ..Default::default()
            },
        )
        .await;
    let second = fx
        .index(
            None,
            repo::NewMessage {
                thread_id: Some(thread),
                ..Default::default()
            },
        )
        .await;
    fx.flag(first, "\\Answered").await;
    let no_thread = fx.index(None, repo::NewMessage::default()).await;

    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(
            &[
                plain_candidate(first),
                plain_candidate(second),
                plain_candidate(no_thread),
            ],
            &plan,
            anchor_now(),
            &no_cancel(),
        )
        .await;

    assert!(out[0].features.user_replied_thread);
    assert!(
        out[1].features.user_replied_thread,
        "same thread as the answered message"
    );
    assert!(!out[2].features.user_replied_thread);
}

// ---------------------------------------------------------------------------
// Personal / global group: contacts, newsletter/automated heuristics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sender_affinity_and_reputation_come_from_contacts_and_saturate() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    fx.seed_contact("alice@example.com", 40, Some(days_ago(now, 1)))
        .await; // above SENDER_VOLUME_SATURATE
    let known = fx
        .index(
            None,
            repo::NewMessage {
                from_addr: Some("alice@example.com".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let unknown = fx
        .index(
            None,
            repo::NewMessage {
                from_addr: Some("stranger@example.com".to_owned()),
                ..Default::default()
            },
        )
        .await;

    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(
            &[plain_candidate(known), plain_candidate(unknown)],
            &plan,
            now,
            &no_cancel(),
        )
        .await;

    let known_f = &out[0].features;
    assert!(
        known_f.sender_affinity > 0.9,
        "saturated volume, very recent contact"
    );
    assert!(known_f.sender_reputation > 0.9);
    let unknown_f = &out[1].features;
    assert_eq!(unknown_f.sender_affinity, 0.0);
    assert_eq!(unknown_f.sender_reputation, 0.0);
}

#[tokio::test]
async fn newsletter_and_automated_heuristics_and_reputation_damping() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    fx.seed_contact("noreply@bigco.com", 40, Some(days_ago(now, 1)))
        .await;
    fx.seed_contact("friend@example.com", 40, Some(days_ago(now, 1)))
        .await;
    let automated = fx
        .index(
            None,
            repo::NewMessage {
                from_addr: Some("noreply@bigco.com".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let newsletter = fx
        .index(
            None,
            repo::NewMessage {
                from_addr: Some("newsletter@shop.com".to_owned()),
                ..Default::default()
            },
        )
        .await;
    let person = fx
        .index(
            None,
            repo::NewMessage {
                from_addr: Some("friend@example.com".to_owned()),
                ..Default::default()
            },
        )
        .await;

    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(
            &[
                plain_candidate(automated),
                plain_candidate(newsletter),
                plain_candidate(person),
            ],
            &plan,
            now,
            &no_cancel(),
        )
        .await;

    assert!(out[0].features.is_automated);
    assert!(!out[0].features.is_newsletter);
    assert!(out[1].features.is_newsletter);
    assert!(!out[2].features.is_automated && !out[2].features.is_newsletter);

    // Same saturated volume/recency for the automated sender and the real
    // correspondent, but the automated one's reputation must be damped.
    assert!(
        out[0].features.sender_reputation < out[2].features.sender_reputation,
        "automated={} person={}",
        out[0].features.sender_reputation,
        out[2].features.sender_reputation
    );
}

// ---------------------------------------------------------------------------
// Structural: msg_length uses the real length, not the scan-capped excerpt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn msg_length_is_the_real_body_length_not_the_scan_cap() {
    let fx = Fixture::open().await;
    let long_body = "x".repeat(MAX_BODY_CHARS_FOR_SCAN as usize + 500);
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                body_text: Some(long_body.clone()),
                ..Default::default()
            },
        )
        .await;
    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(&[plain_candidate(msg)], &plan, anchor_now(), &no_cancel())
        .await;
    assert_eq!(out[0].features.msg_length as usize, long_body.len());
}

// ---------------------------------------------------------------------------
// Temporal: matches_date_intent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn matches_date_intent_requires_every_date_range_to_agree() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    let inside = fx
        .index(
            None,
            repo::NewMessage {
                date: Some(days_ago(now, 3)),
                ..Default::default()
            },
        )
        .await;
    let outside = fx
        .index(
            None,
            repo::NewMessage {
                date: Some(days_ago(now, 30)),
                ..Default::default()
            },
        )
        .await;

    let mut plan = plan_for("");
    plan.hard_filters.push(HardFilter::Date {
        filter: Filter {
            op: Operator::After("2026-05-01".to_owned()),
            negated: false,
        },
        range: DateRange {
            start: Some(days_ago(now, 10)),
            end: None,
        },
    });

    let out = fx
        .extractor()
        .extract_at(
            &[plain_candidate(inside), plain_candidate(outside)],
            &plan,
            now,
            &no_cancel(),
        )
        .await;

    assert!(out[0].features.matches_date_intent);
    assert!(!out[1].features.matches_date_intent);
}

/// A negated date filter (`-after:2026-05-22`, "date scope is *outside*
/// this range") must invert which side reports `true` — dropping
/// `filter.negated` (as an earlier version of `matches_date_intent` did)
/// silently reversed this for every negated date operator.
#[tokio::test]
async fn matches_date_intent_respects_negation() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    let inside = fx
        .index(
            None,
            repo::NewMessage {
                date: Some(days_ago(now, 3)),
                ..Default::default()
            },
        )
        .await;
    let outside = fx
        .index(
            None,
            repo::NewMessage {
                date: Some(days_ago(now, 30)),
                ..Default::default()
            },
        )
        .await;

    let mut plan = plan_for("");
    plan.hard_filters.push(HardFilter::Date {
        filter: Filter {
            op: Operator::After("2026-05-22".to_owned()),
            negated: true, // "NOT after 10 days ago" == "before 10 days ago"
        },
        range: DateRange {
            start: Some(days_ago(now, 10)),
            end: None,
        },
    });

    let out = fx
        .extractor()
        .extract_at(
            &[plain_candidate(inside), plain_candidate(outside)],
            &plan,
            now,
            &no_cancel(),
        )
        .await;

    assert!(
        !out[0].features.matches_date_intent,
        "3 days ago is inside the (non-negated) range, so the negated scope must be false"
    );
    assert!(
        out[1].features.matches_date_intent,
        "30 days ago is outside the range, so the negated scope must be true"
    );
}

#[tokio::test]
async fn matches_date_intent_is_false_with_no_date_scope_expressed() {
    let fx = Fixture::open().await;
    let msg = fx.index(None, repo::NewMessage::default()).await;
    let plan = plan_for(""); // no hard_filters at all
    let out = fx
        .extractor()
        .extract_at(&[plain_candidate(msg)], &plan, anchor_now(), &no_cancel())
        .await;
    assert!(!out[0].features.matches_date_intent);
}

// ---------------------------------------------------------------------------
// Fusion group: pass-through from FusedCandidate/SourceHit, and NaN safety
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fusion_and_semantic_features_pass_through_from_the_fused_candidate() {
    let fx = Fixture::open().await;
    let candidate = FusedCandidate {
        message_id: 987_654, // does not need to exist in the DB
        fused_score: 0.041_5,
        hits: vec![
            SourceHit {
                source: Source::Dense,
                rank: 1,
                score: 0.88,
                mean_score: Some(0.61),
            },
            SourceHit {
                source: Source::Fuzzy,
                rank: 2,
                score: 0.5,
                mean_score: None,
            },
        ],
        num_sources_hit: 2,
        best_source: Source::Dense,
        thread_id: None,
        thread_collapsed: Vec::new(),
        near_duplicates: Vec::new(),
    };
    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(&[candidate], &plan, anchor_now(), &no_cancel())
        .await;
    let f = &out[0].features;

    assert_eq!(f.rrf_score, 0.041_5);
    assert_eq!(f.num_sources_hit, 2);
    assert_eq!(f.best_source, Source::Dense);
    assert_eq!(f.cos_max_chunk, 0.88);
    assert_eq!(f.cos_mean_chunk, 0.61);
    assert_eq!(f.fuzzy_score, 0.5);
    // The message does not exist, so every DB-derived feature degrades
    // honestly rather than panicking — `age_days` is `None` (unknown), not
    // `0.0` (which would misread as "just arrived").
    assert_eq!(f.age_days, None);
    assert_eq!(f.recency_decay, 0.0);
    assert_eq!(f.msg_length, 0);
}

/// A degenerate dense-hit score (e.g. a zero-norm embedding's `0.0 / 0.0`)
/// must not poison the vector or its serialization — `finite` sanitizes it.
#[tokio::test]
async fn degenerate_dense_hit_scores_are_sanitized_not_propagated() {
    let fx = Fixture::open().await;
    let candidate = FusedCandidate {
        message_id: 1,
        // Every one of these has the identical upstream provenance (an
        // externally-supplied `f64` this module does not itself compute)
        // as the dense-hit fields below, and each must be sanitized the
        // same way — not just the two that happened to get a test first.
        fused_score: f64::NAN,
        hits: vec![
            SourceHit {
                source: Source::Dense,
                rank: 1,
                score: f64::NAN,
                mean_score: Some(f64::INFINITY),
            },
            SourceHit {
                source: Source::Fuzzy,
                rank: 2,
                score: f64::NEG_INFINITY,
                mean_score: None,
            },
        ],
        num_sources_hit: 2,
        best_source: Source::Dense,
        thread_id: None,
        thread_collapsed: Vec::new(),
        near_duplicates: Vec::new(),
    };
    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(&[candidate], &plan, anchor_now(), &no_cancel())
        .await;
    let f = &out[0].features;
    assert_eq!(f.cos_max_chunk, 0.0);
    assert_eq!(f.cos_mean_chunk, 0.0);
    assert_eq!(f.fuzzy_score, 0.0);
    assert_eq!(f.rrf_score, 0.0);
    serde_json::to_string(&f).expect("a sanitized vector must always serialize");
}

/// A future-dated message (clock skew, a scheduled send) must clamp to
/// `age_days: Some(0.0)`, not a negative age or `None` — the message's date
/// *is* known, it is simply not in the past yet.
#[tokio::test]
async fn a_future_dated_message_clamps_age_to_zero_not_negative() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                date: Some((now + ChronoDuration::days(5)).timestamp()),
                ..Default::default()
            },
        )
        .await;
    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(&[plain_candidate(msg)], &plan, now, &no_cancel())
        .await;
    assert_eq!(out[0].features.age_days, Some(0.0));
    assert_eq!(
        out[0].features.recency_decay, 1.0,
        "a future date must not extrapolate past maximally recent"
    );
}

// ---------------------------------------------------------------------------
// Robustness: ordering, missing messages, cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn output_is_one_to_one_with_input_in_order() {
    let fx = Fixture::open().await;
    let a = fx.index(None, repo::NewMessage::default()).await;
    let b = fx.index(None, repo::NewMessage::default()).await;
    let plan = plan_for("");
    let out = fx
        .extractor()
        .extract_at(
            &[
                plain_candidate(b),
                plain_candidate(999_999),
                plain_candidate(a),
            ],
            &plan,
            anchor_now(),
            &no_cancel(),
        )
        .await;
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].message_id, b);
    assert_eq!(out[1].message_id, 999_999);
    assert_eq!(out[2].message_id, a);
}

#[tokio::test]
async fn empty_input_returns_empty_output_without_a_single_query() {
    let fx = Fixture::open().await;
    let plan = plan_for("anything");
    let out = fx
        .extractor()
        .extract_at(&[], &plan, anchor_now(), &no_cancel())
        .await;
    assert!(out.is_empty());
}

#[tokio::test]
async fn a_pre_cancelled_token_degrades_every_group_without_panicking() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("invoice".to_owned()),
                date: Some(anchor_now().timestamp()),
                ..Default::default()
            },
        )
        .await;
    fx.flag(msg, "\\Flagged").await;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let plan = plan_for("invoice");
    let out = fx
        .extractor()
        .extract_at(&[plain_candidate(msg)], &plan, anchor_now(), &cancel)
        .await;

    assert_eq!(
        out.len(),
        1,
        "a cancelled lookup still returns one entry per candidate"
    );
    let f = &out[0].features;
    assert_eq!(f.bm25_subject, 0.0);
    assert_eq!(
        f.age_days, None,
        "the core fetch degraded entirely; the date is unknown, not zero"
    );
    assert!(
        !f.is_unread,
        "flags fetch degraded entirely: no claim, not a false 'unread'"
    );
    assert!(!f.is_flagged);
}

// ---------------------------------------------------------------------------
// "Vector completeness": every group actually computes a real value
// ---------------------------------------------------------------------------

/// A rich, fully-seeded candidate exercises every one of prd.md's Stage 3
/// groups at once — textual, semantic, fusion, personal, temporal, status,
/// structural, global — and every field this test checks must be non-default,
/// proving the extractor's computation actually reaches each group rather
/// than the type merely having a slot for it (`features::vector::tests`
/// already covers the latter).
#[tokio::test]
async fn a_fully_seeded_candidate_populates_every_feature_group() {
    let fx = Fixture::open().await;
    let now = anchor_now();
    let thread = fx.insert_thread().await;
    fx.seed_contact("vip@example.com", 15, Some(days_ago(now, 1)))
        .await;

    let msg = fx
        .index(
            None,
            repo::NewMessage {
                subject: Some("Quarterly invoice".to_owned()),
                body_text: Some("please review the invoice details below".to_owned()),
                from_addr: Some("vip@example.com".to_owned()),
                thread_id: Some(thread),
                date: Some(days_ago(now, 2)),
                ..Default::default()
            },
        )
        .await;
    fx.set_thread_root(thread, msg).await;
    fx.set_thread_stats(thread, 2, Some(days_ago(now, 1))).await;
    fx.flag(msg, "\\Flagged").await;

    let candidate = FusedCandidate {
        message_id: msg,
        fused_score: 0.09,
        hits: vec![
            SourceHit {
                source: Source::Lexical,
                rank: 1,
                score: 4.0,
                mean_score: None,
            },
            SourceHit {
                source: Source::Dense,
                rank: 2,
                score: 0.7,
                mean_score: Some(0.5),
            },
        ],
        num_sources_hit: 2,
        best_source: Source::Lexical,
        thread_id: Some(thread),
        thread_collapsed: Vec::new(),
        near_duplicates: Vec::new(),
    };

    let mut plan = plan_for("invoice");
    plan.hard_filters.push(HardFilter::Date {
        filter: Filter {
            op: Operator::After("2026-05-01".to_owned()),
            negated: false,
        },
        range: DateRange {
            start: Some(days_ago(now, 10)),
            end: None,
        },
    });

    let out = fx
        .extractor()
        .extract_at(&[candidate], &plan, now, &no_cancel())
        .await;
    let f = &out[0].features;

    // textual
    assert!(f.bm25_subject > 0.0);
    assert!(f.term_coverage > 0.0);
    assert_eq!(f.best_match_field, MatchField::Subject);
    // semantic
    assert!(f.cos_max_chunk > 0.0);
    assert!(f.cos_mean_chunk > 0.0);
    // fusion
    assert!(f.rrf_score > 0.0);
    assert_eq!(f.num_sources_hit, 2);
    assert_eq!(f.best_source, Source::Lexical);
    // personal
    assert!(f.sender_affinity > 0.0);
    assert!(f.thread_activity > 0.0);
    // temporal
    assert!(f.age_days.is_some_and(|a| a > 0.0));
    assert!(f.recency_decay > 0.0);
    assert!(f.matches_date_intent);
    // status
    assert!(f.is_flagged);
    // structural
    assert!(f.is_thread_root);
    assert_eq!(f.thread_size, 2);
    assert!(f.msg_length > 0);
    // global
    assert!(f.sender_reputation > 0.0);

    // Every feature name still round-trips through `as_pairs` for this
    // real, richly-populated vector.
    let pairs = f.as_pairs();
    assert_eq!(pairs.len(), crate::features::FeatureName::ALL.len());
}
