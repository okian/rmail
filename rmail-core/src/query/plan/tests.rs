//! Coverage per the task's `verify` line: intent labels, spell-fix,
//! expansion, and overall plan shape — plus the date resolution and small
//! pure helpers underneath them.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use chrono::TimeZone;

use super::*;
use crate::config::Bm25Weights;
use crate::embed::hash::HashEmbedder;
use crate::index::{extract_message, FtsIndex, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use crate::repo;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A fixed moment ("now"), so relative dates (`last-week`) and the length of
/// a month are reproducible across runs rather than depending on the day the
/// test happens to execute.
fn fixed_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 5, 12, 0, 0)
        .single()
        .expect("fixed_now: invalid test constant")
}

/// Midnight UTC on a calendar day, as a unix timestamp — an oracle built from
/// chrono's own conversion, so a `DateRange` assertion checks that this
/// module wired the right bound to the right side, not that hand-computed
/// epoch arithmetic agrees with itself.
fn epoch(year: i32, month: u32, day: u32) -> i64 {
    NaiveDate::from_ymd_opt(year, month, day)
        .expect("invalid test date")
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always valid")
        .and_utc()
        .timestamp()
}

struct Fixture {
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
    planner: QueryPlanner,
    account_id: i64,
    mailbox_id: i64,
    next_uid: Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        Self::custom(Arc::new(HashEmbedder::new(64)), ExpansionConfig::default()).await
    }

    async fn with_expansion(expansion: ExpansionConfig) -> Self {
        Self::custom(Arc::new(HashEmbedder::new(64)), expansion).await
    }

    async fn with_embedder(embedder: Arc<dyn Embedder>) -> Self {
        Self::custom(embedder, ExpansionConfig::default()).await
    }

    async fn custom(embedder: Arc<dyn Embedder>, expansion: ExpansionConfig) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-plan-{pid}-{n}.db"));
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
        let planner = QueryPlanner::new(db.clone(), embedder, expansion);
        Self {
            fts: FtsIndex::new(db.clone(), Bm25Weights::default()),
            queue: IndexQueue::new(db.clone(), QueueOptions::default()),
            planner,
            db,
            account_id,
            mailbox_id,
            next_uid: Cell::new(1),
            path,
        }
    }

    /// Store, extract, and lexically index a message — the real indexing
    /// pipeline, so `fts_messages_vocab` (spell-fix, PMI) reflects it exactly
    /// as production would.
    async fn message(
        &self,
        subject: &str,
        body: &str,
        from_addr: &str,
        from_name: Option<&str>,
    ) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            subject: Some(subject.to_owned()),
            body_text: Some(body.to_owned()),
            from_addr: Some(from_addr.to_owned()),
            from_name: from_name.map(str::to_owned),
            ..Default::default()
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

    /// Seed a contact with a specific message count, for boost tests.  Raw
    /// SQL rather than `repo::upsert_contact`, which only ever increments by
    /// one per call.
    async fn contact(&self, address: &str, name: Option<&str>, message_count: i64) {
        let address = address.to_owned();
        let name = name.map(str::to_owned);
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO contacts (address, name, message_count, last_seen)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![address, name, message_count, 0],
                )
            })
            .await
            .expect("seed contact");
    }

    /// Four messages containing both "invoice" and "receipt", plus filler
    /// messages containing neither — a corpus shaped so the two co-occur far
    /// more than chance predicts.
    async fn seed_invoice_receipt_corpus(&self) {
        for _ in 0..4 {
            self.message("Invoice", "invoice receipt", "billing@acme.com", None)
                .await;
        }
        for _ in 0..12 {
            self.message(
                "Weekly sync",
                "meeting notes team sync",
                "team@example.com",
                None,
            )
            .await;
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// An embedder that always fails, for the graceful-degradation test.
#[derive(Debug)]
struct FailingEmbedder;

#[async_trait::async_trait]
impl Embedder for FailingEmbedder {
    fn model(&self) -> &str {
        "failing-test-double"
    }

    fn dim(&self) -> usize {
        8
    }

    async fn embed(&self, _texts: &[String]) -> Result<Vec<Embedding>, Error> {
        Err(Error::unavailable("embedder offline"))
    }

    async fn warm(&self) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Date resolution
// ---------------------------------------------------------------------------

#[test]
fn an_iso_date_resolves_to_its_single_day() {
    let (start, end) =
        resolve_date_span("2026-07-01", fixed_now()).expect("2026-07-01 should resolve");
    assert_eq!(
        start.date_naive(),
        NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
    );
    assert_eq!(
        end.date_naive(),
        NaiveDate::from_ymd_opt(2026, 7, 2).unwrap()
    );
}

#[test]
fn a_year_month_resolves_to_the_whole_month() {
    let (start, end) = resolve_date_span("2025-06", fixed_now()).expect("2025-06 should resolve");
    assert_eq!(
        start.date_naive(),
        NaiveDate::from_ymd_opt(2025, 6, 1).unwrap()
    );
    assert_eq!(
        end.date_naive(),
        NaiveDate::from_ymd_opt(2025, 7, 1).unwrap()
    );
}

#[test]
fn a_bare_year_resolves_to_the_whole_year() {
    let (start, end) = resolve_date_span("2025", fixed_now()).expect("2025 should resolve");
    assert_eq!(
        start.date_naive(),
        NaiveDate::from_ymd_opt(2025, 1, 1).unwrap()
    );
    assert_eq!(
        end.date_naive(),
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
    );
}

#[test]
fn today_and_yesterday_are_relative_to_now() {
    let now = fixed_now();
    let (start, end) = resolve_date_span("today", now).expect("today should resolve");
    assert_eq!(start.date_naive(), now.date_naive());
    assert_eq!(end.date_naive(), now.date_naive() + Duration::days(1));

    let (ystart, _) = resolve_date_span("yesterday", now).expect("yesterday should resolve");
    assert_eq!(ystart.date_naive(), now.date_naive() - Duration::days(1));
}

#[test]
fn this_week_and_last_week_start_on_monday_and_are_a_week_apart() {
    let now = fixed_now();
    let (start, end) = resolve_date_span("this-week", now).expect("this-week should resolve");
    let week = now.date_naive().week(Weekday::Mon);
    assert_eq!(start.date_naive(), week.first_day());
    assert_eq!(end.date_naive(), week.last_day() + Duration::days(1));

    let (last_start, last_end) =
        resolve_date_span("last-week", now).expect("last-week should resolve");
    assert_eq!(last_start, start - Duration::weeks(1));
    assert_eq!(last_end, end - Duration::weeks(1));
}

#[test]
fn underscore_and_space_variants_of_a_relative_date_agree() {
    let now = fixed_now();
    assert_eq!(
        resolve_date_span("last-week", now),
        resolve_date_span("last_week", now)
    );
    assert_eq!(
        resolve_date_span("last-week", now),
        resolve_date_span("Last Week", now)
    );
}

#[test]
fn last_month_wraps_across_a_year_boundary() {
    let now = Utc
        .with_ymd_and_hms(2026, 1, 15, 0, 0, 0)
        .single()
        .expect("last_month_wraps: invalid test constant");
    let (start, end) = resolve_date_span("last-month", now).expect("last-month should resolve");
    assert_eq!(
        start.date_naive(),
        NaiveDate::from_ymd_opt(2025, 12, 1).unwrap()
    );
    assert_eq!(
        end.date_naive(),
        NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()
    );
}

#[test]
fn an_unrecognized_date_expression_does_not_resolve() {
    assert!(resolve_date_span("whenever", fixed_now()).is_none());
    assert!(resolve_date_span("", fixed_now()).is_none());
    assert!(resolve_date_span("2025-13", fixed_now()).is_none()); // no month 13
}

#[tokio::test]
async fn before_after_on_and_range_operators_resolve_into_hard_filters() {
    let fx = Fixture::open().await;
    let plan = fx
        .planner
        .plan_at(
            "before:2026-07-01 after:2026-01-01 on:2026-03-15 date:2025-06..2025-08",
            fixed_now(),
        )
        .await
        .expect("plan_at should assemble");
    assert_eq!(plan.hard_filters.len(), 4);

    for hard_filter in &plan.hard_filters {
        let HardFilter::Date { filter, range } = hard_filter else {
            unreachable!(
                "every filter in this query is date-shaped and resolvable: {hard_filter:?}"
            );
        };
        match &filter.op {
            // `before:`/`after:` anchor on the *start* of the named day —
            // see `resolve_filters`'s doc comment — so the exact epoch is
            // worth asserting, not just which side is `Some`: an off-by-a-
            // day flip on the bound would still pass a `Some`/`None`-only
            // check.
            Operator::Before(_) => {
                assert_eq!(range.start, None);
                assert_eq!(range.end, Some(epoch(2026, 7, 1)));
            }
            Operator::After(_) => {
                assert_eq!(range.start, Some(epoch(2026, 1, 1)));
                assert_eq!(range.end, None);
            }
            Operator::On(_) => {
                assert_eq!(range.start, Some(epoch(2026, 3, 15)));
                assert_eq!(range.end, Some(epoch(2026, 3, 16)));
            }
            Operator::DateRange(_, _) => {
                assert_eq!(range.start, Some(epoch(2025, 6, 1)));
                assert_eq!(range.end, Some(epoch(2025, 9, 1)));
            }
            other => unreachable!("unexpected operator in this query: {other:?}"),
        }
    }
}

#[tokio::test]
async fn an_unresolvable_date_filter_keeps_the_filter_but_drops_the_resolution() {
    let fx = Fixture::open().await;
    let plan = fx
        .planner
        .plan_at("before:whenever", fixed_now())
        .await
        .expect("plan_at should assemble");
    assert_eq!(plan.hard_filters.len(), 1);
    // An unresolvable date is indistinguishable, at the type level, from any
    // other non-date operator -- see `HardFilter`'s docs on why that is the
    // point, not an oversight.
    match &plan.hard_filters[0] {
        HardFilter::Other(filter) => {
            // Degrading never loses what the user typed.
            assert_eq!(filter.op, Operator::Before("whenever".to_owned()));
        }
        other => unreachable!("an unresolvable date must not become HardFilter::Date: {other:?}"),
    }
}

#[tokio::test]
async fn a_range_with_one_unresolvable_bound_drops_the_whole_constraint() {
    let fx = Fixture::open().await;
    let plan = fx
        .planner
        .plan_at("date:2025-06..whenever", fixed_now())
        .await
        .expect("plan_at should assemble");
    assert_eq!(plan.hard_filters.len(), 1);
    assert!(matches!(plan.hard_filters[0], HardFilter::Other(_)));
}

#[tokio::test]
async fn an_inverted_range_drops_the_constraint_rather_than_matching_nothing_forever() {
    let fx = Fixture::open().await;
    let plan = fx
        .planner
        .plan_at("date:2025-08..2025-06", fixed_now())
        .await
        .expect("plan_at should assemble");
    assert_eq!(plan.hard_filters.len(), 1);
    assert!(
        matches!(plan.hard_filters[0], HardFilter::Other(_)),
        "start > end must degrade like any other malformed date, not become an \
         unsatisfiable range"
    );
}

// ---------------------------------------------------------------------------
// Spell-fix
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_misspelled_word_is_corrected_and_boosted_alongside_the_original() {
    let fx = Fixture::open().await;
    for i in 0..3 {
        fx.message(
            &format!("Invoice from Acme #{i}"),
            "please find the invoice attached",
            "billing@acme.com",
            Some("Acme Billing"),
        )
        .await;
    }

    let plan = fx
        .planner
        .plan("invoce")
        .await
        .expect("plan should assemble");

    let original = plan
        .lexical_terms
        .iter()
        .find(|t| t.text == "invoce")
        .expect("original term must survive a correction");
    assert_eq!(original.origin, TermOrigin::Original);
    assert_eq!(original.weight, 1.0);

    let corrected = plan
        .lexical_terms
        .iter()
        .find(|t| t.text == "invoice")
        .expect("a confident correction should have been added");
    match &corrected.origin {
        TermOrigin::SpellFixed { from } => assert_eq!(from, "invoce"),
        other => unreachable!("expected a spell-fix origin, got {other:?}"),
    }
    assert!(
        corrected.weight > original.weight,
        "prd.md: corrected terms are boosted over the original"
    );
}

#[tokio::test]
async fn an_exact_vocabulary_word_is_not_corrected() {
    let fx = Fixture::open().await;
    fx.message("Invoice", "invoice details", "billing@acme.com", None)
        .await;
    fx.message("Invoice again", "invoice pending", "billing@acme.com", None)
        .await;

    let plan = fx
        .planner
        .plan("invoice")
        .await
        .expect("plan should assemble");
    assert_eq!(
        plan.lexical_terms.len(),
        1,
        "a correctly-spelled term must not gain a spurious sibling"
    );
    assert_eq!(plan.lexical_terms[0].origin, TermOrigin::Original);
}

#[tokio::test]
async fn a_negated_term_is_never_spell_fixed() {
    let fx = Fixture::open().await;
    fx.message("Invoice", "invoice details", "billing@acme.com", None)
        .await;
    fx.message("Invoice again", "invoice pending", "billing@acme.com", None)
        .await;

    let plan = fx
        .planner
        .plan("-invoce")
        .await
        .expect("plan should assemble");
    assert_eq!(
        plan.lexical_terms.len(),
        1,
        "correcting an excluded term would change what gets excluded"
    );
    assert!(plan.lexical_terms[0].negated);
}

#[tokio::test]
async fn spellfix_can_be_disabled_by_config() {
    let fx = Fixture::with_expansion(ExpansionConfig {
        spellfix: false,
        ..ExpansionConfig::default()
    })
    .await;
    fx.message("Invoice", "invoice details", "billing@acme.com", None)
        .await;
    fx.message("Invoice again", "invoice pending", "billing@acme.com", None)
        .await;

    let plan = fx
        .planner
        .plan("invoce")
        .await
        .expect("plan should assemble");
    assert_eq!(plan.lexical_terms.len(), 1);
    assert_eq!(plan.lexical_terms[0].text, "invoce");
}

#[tokio::test]
async fn a_lexical_or_semantic_sigil_is_never_spell_fixed() {
    // parse.rs's module docs: `=` "bypasses semantic recall and query
    // expansion". Spell-fix adds a second term the user did not type, which
    // is exactly query expansion -- so `=invoce`/`~invoce` must stay exactly
    // as typed even though "invoice" is a confident, well-attested
    // correction for the unmoded case (see the test above).
    let fx = Fixture::open().await;
    for i in 0..3 {
        fx.message(
            &format!("Invoice from Acme #{i}"),
            "please find the invoice attached",
            "billing@acme.com",
            Some("Acme Billing"),
        )
        .await;
    }

    for query in ["=invoce", "~invoce"] {
        let plan = fx
            .planner
            .plan(query)
            .await
            .unwrap_or_else(|e| unreachable!("plan {query:?}: {e}"));
        assert_eq!(
            plan.lexical_terms.len(),
            1,
            "{query:?} must not gain a spell-fix sibling"
        );
        assert_eq!(plan.lexical_terms[0].origin, TermOrigin::Original);
    }
}

#[test]
fn edit_distance_counts_a_single_insertion() {
    assert_eq!(edit_distance("invoce", "invoice"), 1);
    assert_eq!(edit_distance("invoice", "invoice"), 0);
    assert_eq!(edit_distance("cat", "dog"), 3);
}

// ---------------------------------------------------------------------------
// PMI synonym expansion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn synonyms_are_surfaced_from_local_term_cooccurrence() {
    let fx = Fixture::open().await;
    fx.seed_invoice_receipt_corpus().await;

    let plan = fx
        .planner
        .plan("invoice")
        .await
        .expect("plan should assemble");

    let synonym = plan
        .expansions
        .iter()
        .find(|t| t.text == "receipt")
        .expect("receipt should surface as a synonym of invoice");
    match &synonym.origin {
        TermOrigin::Synonym { from } => assert_eq!(from, "invoice"),
        other => unreachable!("expected a synonym origin, got {other:?}"),
    }
    assert!(
        synonym.weight < ORIGINAL_WEIGHT,
        "prd.md: expansions are soft and down-weighted"
    );
}

#[tokio::test]
async fn a_synonym_already_typed_is_not_duplicated_as_an_expansion() {
    let fx = Fixture::open().await;
    fx.seed_invoice_receipt_corpus().await;

    let plan = fx
        .planner
        .plan("invoice receipt")
        .await
        .expect("plan should assemble");
    assert!(
        plan.expansions.iter().all(|t| t.text != "receipt"),
        "a word the user already typed is not new recall evidence"
    );
}

#[tokio::test]
async fn a_lexical_sigil_term_does_not_pivot_synonym_expansion() {
    // Same sigil contract as spell-fix (see the `plan.rs`-side note on
    // `expand_synonyms`): `=invoice` must not surface "receipt" as a
    // synonym, even from a corpus shaped to make that expansion fire for a
    // plain `invoice` (the test above).
    let fx = Fixture::open().await;
    fx.seed_invoice_receipt_corpus().await;

    let plan = fx
        .planner
        .plan("=invoice")
        .await
        .expect("plan should assemble");
    assert!(plan.expansions.is_empty());
}

#[tokio::test]
async fn synonym_expansion_can_be_disabled_by_config() {
    let fx = Fixture::with_expansion(ExpansionConfig {
        synonyms: false,
        ..ExpansionConfig::default()
    })
    .await;
    fx.seed_invoice_receipt_corpus().await;

    let plan = fx
        .planner
        .plan("invoice")
        .await
        .expect("plan should assemble");
    assert!(plan.expansions.is_empty());
}

#[tokio::test]
async fn a_pivot_with_no_stable_cooccurrence_expands_to_nothing() {
    let fx = Fixture::open().await;
    // A single occurrence anywhere else in the corpus so total_docs > 0, but
    // "solitary" never repeats or co-occurs with anything.
    fx.message("Note", "solitary word here", "team@example.com", None)
        .await;

    let plan = fx
        .planner
        .plan("solitary")
        .await
        .expect("plan should assemble");
    assert!(plan.expansions.is_empty());
}

// ---------------------------------------------------------------------------
// Intent classification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn intent_classification_matches_the_prds_worked_examples() {
    let fx = Fixture::open().await;
    // Deliberately not sharing a substring with any lexicon word this test
    // exercises: "billing" would substring-match the lookup-lexicon word
    // "bill" (the "AWS bill" case below), silently giving that query a
    // `has_known_contact` feature it should not have and changing which
    // intent wins.
    fx.contact("hello@acmecorp.io", Some("Acme Corp"), 10).await;

    let cases: &[(&str, Intent)] = &[
        // prd.md, Stage 0 step 2's own examples:
        ("the invoice Acme sent last week", Intent::Navigational),
        ("everything about the office move", Intent::Exploratory),
        ("tracking number for my order", Intent::Lookup),
        ("AWS bill", Intent::Lookup),
        // Operator-only queries are known-item by construction.
        ("from:alice tag:work", Intent::Navigational),
        // prd.md's own "genuinely ambiguous natural language" example.
        (
            "who did I forget to reply to about the lease",
            Intent::Exploratory,
        ),
    ];
    for (query, expected) in cases {
        let plan = fx
            .planner
            .plan(query)
            .await
            .unwrap_or_else(|e| unreachable!("plan {query:?}: {e}"));
        assert_eq!(plan.intent, *expected, "query: {query:?}");
    }
}

#[tokio::test]
async fn prose_without_operators_or_phrases_needs_nl_compile() {
    let fx = Fixture::open().await;

    let prose = fx
        .planner
        .plan("who did I forget to reply to about the lease")
        .await
        .expect("plan should assemble");
    assert!(prose.needs_nl_compile);

    let keyword = fx
        .planner
        .plan("invoice acme")
        .await
        .expect("plan should assemble");
    assert!(!keyword.needs_nl_compile);

    let operator = fx
        .planner
        .plan("from:alice invoice")
        .await
        .expect("plan should assemble");
    assert!(!operator.needs_nl_compile);

    let quoted = fx
        .planner
        .plan("\"who cares\" invoice")
        .await
        .expect("plan should assemble");
    assert!(!quoted.needs_nl_compile);
}

// ---------------------------------------------------------------------------
// Entities: patterns and contacts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entity_patterns_are_recognized_without_a_database() {
    let fx = Fixture::open().await;
    let plan = fx
        .planner
        .plan("write to ada@example.com about invoice INV-2024-0231")
        .await
        .expect("plan should assemble");

    assert!(plan.entities.iter().any(
        |e| e.kind == EntityRefKind::Pattern(EntityKind::Email) && e.norm == "ada@example.com"
    ));
    assert!(plan
        .entities
        .iter()
        .any(|e| e.kind == EntityRefKind::Pattern(EntityKind::InvoiceId)));
}

#[tokio::test]
async fn a_literal_email_and_its_resolved_contact_are_not_double_counted() {
    // A bare `bob@example.com` in the query is both an entity *pattern*
    // (task 19's email regex) and, once matched against the contact graph,
    // a *contact* -- the same underlying address reached two ways. A
    // retriever that sums `boost` across `entities` must see it once.
    let fx = Fixture::open().await;
    fx.contact("bob@example.com", Some("Bob Jones"), 20).await;
    let plan = fx
        .planner
        .plan("bob@example.com")
        .await
        .expect("plan should assemble");

    let matches: Vec<&EntityRef> = plan
        .entities
        .iter()
        .filter(|e| e.norm == "bob@example.com")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "the pattern match and the contact match are one signal, not two: {matches:?}"
    );
    // The contact wins even though a saturated contact's boost (0.5, at
    // `message_count = 20`) is numerically smaller than a pattern-matched
    // email's (0.6) -- it carries a display name a bare pattern match does
    // not, which dedup must not throw away just because the number was
    // bigger.
    assert_eq!(matches[0].kind, EntityRefKind::Contact);
    assert_eq!(matches[0].display.as_deref(), Some("Bob Jones"));
}

#[test]
fn contact_boost_saturates_rather_than_growing_without_bound() {
    assert_eq!(contact_boost(0), 0.0);
    assert!(contact_boost(1) > 0.0);
    assert_eq!(contact_boost(CONTACT_BOOST_SATURATE), CONTACT_BOOST_MAX);
    assert_eq!(
        contact_boost(CONTACT_BOOST_SATURATE * 100),
        CONTACT_BOOST_MAX,
        "a hyperactive contact must not dominate every query that matches their name"
    );
}

#[tokio::test]
async fn a_contact_resolved_from_free_text_is_a_soft_boost_not_a_filter() {
    let fx = Fixture::open().await;
    fx.contact("bob@example.com", Some("Bob Jones"), 5).await;

    let plan = fx
        .planner
        .plan("lunch with bob next week")
        .await
        .expect("plan should assemble");

    let bob = plan
        .entities
        .iter()
        .find(|e| e.kind == EntityRefKind::Contact)
        .expect("bob should resolve to a contact");
    assert_eq!(bob.norm, "bob@example.com");
    assert_eq!(bob.display.as_deref(), Some("Bob Jones"));
    assert!(bob.boost > 0.0 && bob.boost <= CONTACT_BOOST_MAX);
    // Never promoted to a hard filter: the user did not type `from:bob`.
    assert!(!plan.hard_filters.iter().any(|f| matches!(
        &f.filter().op,
        Operator::From(v) | Operator::To(v) if v == "bob"
    )));
}

#[tokio::test]
async fn an_operator_shaped_degraded_token_is_not_also_resolved_as_a_contact() {
    let fx = Fixture::open().await;
    // A registered key with a value that doesn't fit its shape degrades to
    // free text (parse.rs) but keeps `looked_like_operator`; that token
    // already tried and failed one reading and should not get a second guess
    // as a contact name. `parse.rs` degrades to the *whole original token*
    // (`"larger:bob"`, not just `"bob"`), so the contact seeded here has to
    // contain that whole string -- a contact merely named "Bob" would never
    // substring-match "larger:bob" regardless of whether the guard exists,
    // which would make this test pass for the wrong reason.
    fx.contact("hello@largerbob.example", Some("larger:bob Ltd"), 5)
        .await;
    let plan = fx
        .planner
        .plan("larger:bob")
        .await
        .expect("plan should assemble");
    assert!(
        !plan
            .entities
            .iter()
            .any(|e| e.kind == EntityRefKind::Contact),
        "a degraded operator token must not get a second reading as a contact name"
    );
}

// ---------------------------------------------------------------------------
// Overall plan shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plan_shape_covers_every_field() {
    let fx = Fixture::open().await;
    fx.contact("bob@example.com", Some("Bob Jones"), 5).await;

    let raw =
        "from:bob account:Work in:INBOX \"quarterly report\" bob@example.com invoice INV-2024-0231";
    let plan = fx.planner.plan(raw).await.expect("plan should assemble");

    assert_eq!(plan.raw, raw);

    assert!(plan
        .hard_filters
        .iter()
        .any(|f| matches!(f.filter().op, Operator::From(_))));

    assert_eq!(plan.phrases.len(), 1);
    assert_eq!(plan.phrases[0].text, "quarterly report");

    assert!(plan.query_vector.is_some());

    assert_eq!(plan.sort, SortSpec::Relevance);

    assert_eq!(plan.scope.accounts, vec!["Work".to_owned()]);
    assert_eq!(plan.scope.mailboxes, vec!["INBOX".to_owned()]);

    assert!(plan
        .entities
        .iter()
        .any(|e| matches!(e.kind, EntityRefKind::Pattern(EntityKind::InvoiceId))));
    assert!(plan
        .entities
        .iter()
        .any(|e| e.kind == EntityRefKind::Contact && e.norm == "bob@example.com"));
}

#[tokio::test]
async fn a_filters_only_query_has_no_vector_to_embed() {
    let fx = Fixture::open().await;
    let plan = fx
        .planner
        .plan("from:alice tag:work")
        .await
        .expect("plan should assemble");
    assert!(plan.query_vector.is_none());
    assert!(plan.lexical_terms.is_empty());
}

#[tokio::test]
async fn a_failed_embedder_degrades_to_no_vector_rather_than_failing_the_plan() {
    let fx = Fixture::with_embedder(Arc::new(FailingEmbedder)).await;
    let plan = fx
        .planner
        .plan("invoice acme")
        .await
        .expect("plan must still assemble even though embedding fails");
    assert!(plan.query_vector.is_none());
    assert!(
        !plan.lexical_terms.is_empty(),
        "the rest of the plan assembles even when embedding fails"
    );
}

#[tokio::test]
async fn an_empty_query_produces_an_empty_but_valid_plan() {
    let fx = Fixture::open().await;
    let plan = fx.planner.plan("").await.expect("plan should assemble");
    assert!(plan.hard_filters.is_empty());
    assert!(plan.lexical_terms.is_empty());
    assert!(plan.phrases.is_empty());
    assert!(plan.expansions.is_empty());
    assert!(plan.query_vector.is_none());
    assert_eq!(plan.sort, SortSpec::Relevance);
}

// ---------------------------------------------------------------------------
// Small pure helpers
// ---------------------------------------------------------------------------

#[test]
fn escape_like_neutralizes_wildcards() {
    assert_eq!(escape_like("50%_off"), "50\\%\\_off");
    assert_eq!(escape_like("plain"), "plain");
}

#[test]
fn question_words_are_detected_by_first_word_only() {
    assert!(starts_with_question_word("who did this"));
    assert!(starts_with_question_word("Who did this"));
    assert!(!starts_with_question_word("the invoice who sent it"));
    assert!(!starts_with_question_word(""));
}

#[test]
fn relative_date_phrases_are_found_inside_free_text() {
    assert!(contains_relative_date_phrase("the invoice sent last week"));
    assert!(!contains_relative_date_phrase("the invoice sent recently"));
}
