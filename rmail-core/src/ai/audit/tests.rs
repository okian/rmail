//! The properties the audit ledger exists for: every field of a call is
//! recorded faithfully (including a hash that actually matches the bytes
//! sent), the table cannot be edited or erased once written, and the day
//! rollup tracks the ledger exactly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use chrono::TimeZone;
use sha2::Digest;

use super::*;
use crate::repo;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDb {
    db: Database,
    path: PathBuf,
}

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-audit-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        Self { db, path }
    }

    /// A real account/message pair, so tests that want plausible ids don't
    /// have to invent them. `ai_ledger` does not enforce these as foreign
    /// keys (see the migration), so any `i64` would do, but exercising the
    /// happy path with rows that actually exist is worth the two inserts.
    async fn account_and_message(&self) -> (i64, i64) {
        self.db
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
                let message_id = repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid: 1,
                        uidvalidity: 1,
                        subject: Some("Hello".to_owned()),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, message_id))
            })
            .await
            .unwrap()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn sample_usage() -> Usage {
    Usage {
        input_tokens: 1_200,
        output_tokens: 340,
        cache_creation_input_tokens: 900,
        cache_read_input_tokens: 4_500,
    }
}

/// Insert a ledger row at an arbitrary `created_at`, bypassing `record_call`.
///
/// Only for tests of the `since`/`until` filter: `record_call` always stamps
/// the real clock, and once a row exists the append-only triggers correctly
/// refuse to let anything — including a test — backdate it afterward.
async fn seed_at(db: &Database, created_at: i64, model: &str) {
    let model = model.to_owned();
    db.write(move |c| {
        c.execute(
            "INSERT INTO ai_ledger (
                created_at, model, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                redaction_level, latency_ms, payload_sha256, status
             ) VALUES (?1, ?2, 0, 0, 0, 0, 0.0, 'none', 0, ?3, 'ok')",
            rusqlite::params![created_at, model, vec![0u8]],
        )
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn record_call_persists_every_field_faithfully() {
    let tmp = TempDb::open();
    let (account_id, message_id) = tmp.account_and_message().await;
    let payload = b"redacted request body, ready to leave the machine";

    let id = record_call(
        &tmp.db,
        CallRecord {
            account_id: Some(account_id),
            message_id: Some(message_id),
            request_id: Some("msg_01abc".to_owned()),
            model: "claude-haiku-4-5".to_owned(),
            pass: Some("triage".to_owned()),
            usage: sample_usage(),
            redaction_level: "redacted".to_owned(),
            latency: Duration::from_millis(842),
            payload,
            outcome: CallOutcome::Ok,
        },
    )
    .await
    .unwrap();

    let entries = query_calls(&tmp.db, &AuditFilter::default(), 10, None)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];

    assert_eq!(entry.id, id);
    assert_eq!(entry.account_id, Some(account_id));
    assert_eq!(entry.message_id, Some(message_id));
    assert_eq!(entry.request_id.as_deref(), Some("msg_01abc"));
    assert_eq!(entry.model, "claude-haiku-4-5");
    assert_eq!(entry.pass.as_deref(), Some("triage"));
    assert_eq!(entry.usage, sample_usage());
    assert_eq!(entry.redaction_level, "redacted");
    assert_eq!(entry.latency_ms, 842);
    assert_eq!(entry.status, CallStatus::Ok);
    assert_eq!(entry.error, None);
    // The hash proves what left the machine: it must match the exact bytes
    // handed to `record_call`, computed independently here rather than by
    // re-deriving it the same way the code under test does.
    assert_eq!(entry.payload_sha256, sha2::Sha256::digest(payload).to_vec());
}

#[tokio::test]
async fn payload_hash_changes_when_the_transmitted_bytes_change() {
    // Guards against a hash that is a constant, a length, or otherwise not
    // actually keyed on the payload — the whole point of storing it is that
    // two different payloads are distinguishable.
    let tmp = TempDb::open();
    let record = |payload: &'static [u8]| CallRecord {
        account_id: None,
        message_id: None,
        request_id: None,
        model: "claude-haiku-4-5".to_owned(),
        pass: None,
        usage: sample_usage(),
        redaction_level: "none".to_owned(),
        latency: Duration::from_millis(1),
        payload,
        outcome: CallOutcome::Ok,
    };

    record_call(&tmp.db, record(b"first payload"))
        .await
        .unwrap();
    record_call(&tmp.db, record(b"a different payload"))
        .await
        .unwrap();

    let entries = query_calls(&tmp.db, &AuditFilter::default(), 10, None)
        .await
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert_ne!(entries[0].payload_sha256, entries[1].payload_sha256);
}

#[tokio::test]
async fn record_call_records_an_error_outcome() {
    let tmp = TempDb::open();

    record_call(
        &tmp.db,
        CallRecord {
            account_id: None,
            message_id: None,
            request_id: None,
            model: "claude-opus-4-8".to_owned(),
            pass: Some("deep".to_owned()),
            usage: Usage::default(),
            redaction_level: "none".to_owned(),
            latency: Duration::from_millis(50),
            payload: b"",
            outcome: CallOutcome::Error("upstream 529: overloaded".to_owned()),
        },
    )
    .await
    .unwrap();

    let entries = query_calls(&tmp.db, &AuditFilter::default(), 10, None)
        .await
        .unwrap();
    assert_eq!(entries[0].status, CallStatus::Error);
    assert_eq!(
        entries[0].error.as_deref(),
        Some("upstream 529: overloaded")
    );
}

// ---------------------------------------------------------------------------
// Append-only invariant — the point of this task. These assert the database
// itself rejects the write, not merely that this module never issues one.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ai_ledger_rejects_update_at_the_database_layer() {
    let tmp = TempDb::open();
    let id = record_call(
        &tmp.db,
        CallRecord {
            account_id: None,
            message_id: None,
            request_id: None,
            model: "claude-haiku-4-5".to_owned(),
            pass: None,
            usage: sample_usage(),
            redaction_level: "none".to_owned(),
            latency: Duration::from_millis(10),
            payload: b"x",
            outcome: CallOutcome::Ok,
        },
    )
    .await
    .unwrap();

    // A raw UPDATE against the table, issued directly rather than through any
    // function this module exposes — if the trigger were missing, this would
    // silently succeed and corrupt the audit trail.
    let result = tmp.db.with_write(|c| {
        c.execute(
            "UPDATE ai_ledger SET model = 'tampered' WHERE id = ?1",
            [id],
        )
    });
    let err = result.expect_err("UPDATE against ai_ledger must be rejected");
    assert!(
        err.to_string().contains("append-only"),
        "unexpected error: {err}"
    );

    // The row is provably untouched, not just "the statement errored for some
    // other reason."
    let entries = query_calls(&tmp.db, &AuditFilter::default(), 10, None)
        .await
        .unwrap();
    assert_eq!(entries[0].model, "claude-haiku-4-5");
}

#[tokio::test]
async fn ai_ledger_rejects_delete_at_the_database_layer() {
    let tmp = TempDb::open();
    let id = record_call(
        &tmp.db,
        CallRecord {
            account_id: None,
            message_id: None,
            request_id: None,
            model: "claude-sonnet-5".to_owned(),
            pass: None,
            usage: sample_usage(),
            redaction_level: "none".to_owned(),
            latency: Duration::from_millis(10),
            payload: b"x",
            outcome: CallOutcome::Ok,
        },
    )
    .await
    .unwrap();

    let result = tmp
        .db
        .with_write(|c| c.execute("DELETE FROM ai_ledger WHERE id = ?1", [id]));
    let err = result.expect_err("DELETE against ai_ledger must be rejected");
    assert!(
        err.to_string().contains("append-only"),
        "unexpected error: {err}"
    );

    let entries = query_calls(&tmp.db, &AuditFilter::default(), 10, None)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1, "the row must still be there");
}

#[tokio::test]
async fn ai_ledger_rejects_insert_or_replace_over_an_existing_id() {
    // `INSERT OR REPLACE` is an INSERT, not an UPDATE or a DELETE — SQLite
    // resolves the id collision by deleting the old row internally, and that
    // internal delete does not fire a `BEFORE DELETE` trigger unless
    // `PRAGMA recursive_triggers` is on (this codebase's `configure_writer`
    // does not set it). Without a dedicated guard, this statement would
    // silently rewrite a row's model *and its payload hash* — exactly the
    // tamper this ledger exists to make impossible — while both triggers
    // above stayed silent. This test would have caught that gap.
    let tmp = TempDb::open();
    let payload = b"the original, hashed payload";
    let id = record_call(
        &tmp.db,
        CallRecord {
            account_id: None,
            message_id: None,
            request_id: None,
            model: "claude-haiku-4-5".to_owned(),
            pass: None,
            usage: sample_usage(),
            redaction_level: "none".to_owned(),
            latency: Duration::from_millis(10),
            payload,
            outcome: CallOutcome::Ok,
        },
    )
    .await
    .unwrap();

    let result = tmp.db.with_write(|c| {
        c.execute(
            "INSERT OR REPLACE INTO ai_ledger (
                id, created_at, model, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_read_input_tokens, cost_usd,
                redaction_level, latency_ms, payload_sha256, status
             ) VALUES (?1, 0, 'tampered', 0, 0, 0, 0, 0, 'none', 0, X'FFFF', 'ok')",
            [id],
        )
    });
    let err = result.expect_err("INSERT OR REPLACE over an existing id must be rejected");
    assert!(
        err.to_string().contains("append-only"),
        "unexpected error: {err}"
    );

    // Both the model and the payload hash — the two things a tamper would
    // target — must be provably unchanged, not merely "an error happened."
    let entries = query_calls(&tmp.db, &AuditFilter::default(), 10, None)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].model, "claude-haiku-4-5");
    assert_eq!(
        entries[0].payload_sha256,
        sha2::Sha256::digest(payload).to_vec()
    );
}

// ---------------------------------------------------------------------------
// ai_usage rollups
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ai_usage_rolls_up_every_call_recorded_today() {
    let tmp = TempDb::open();

    for model in ["claude-haiku-4-5", "claude-haiku-4-5", "claude-opus-4-8"] {
        record_call(
            &tmp.db,
            CallRecord {
                account_id: None,
                message_id: None,
                request_id: None,
                model: model.to_owned(),
                pass: None,
                usage: sample_usage(),
                redaction_level: "none".to_owned(),
                latency: Duration::from_millis(10),
                payload: b"x",
                outcome: CallOutcome::Ok,
            },
        )
        .await
        .unwrap();
    }

    // Derive "today" from a row this test actually wrote, rather than a
    // second, independent read of the clock: `record_call` stamps
    // `created_at` from `Utc::now()` at write time, so re-reading the clock
    // here — even after the writes — is still a race against UTC midnight
    // landing between the two reads. Reading it back off a written row has
    // no such window.
    let written = query_calls(&tmp.db, &AuditFilter::default(), 1, None)
        .await
        .unwrap();
    let today = day_key(written[0].created_at);
    let usage = usage_for_day(&tmp.db, &today).await.unwrap().unwrap();
    assert_eq!(usage.requests, 3);
    assert_eq!(
        usage.input_tokens,
        3 * i64::from(sample_usage().input_tokens)
    );
    assert_eq!(
        usage.output_tokens,
        3 * i64::from(sample_usage().output_tokens)
    );
    assert_eq!(
        usage.cache_creation_input_tokens,
        3 * i64::from(sample_usage().cache_creation_input_tokens)
    );
    assert_eq!(
        usage.cache_read_input_tokens,
        3 * i64::from(sample_usage().cache_read_input_tokens)
    );
    let expected_cost = 2.0 * estimate_cost_usd("claude-haiku-4-5", sample_usage())
        + estimate_cost_usd("claude-opus-4-8", sample_usage());
    assert!(
        (usage.cost_usd - expected_cost).abs() < 1e-9,
        "cost_usd = {}, expected {}",
        usage.cost_usd,
        expected_cost
    );
}

#[tokio::test]
async fn ai_usage_keeps_different_days_separate() {
    let tmp = TempDb::open();

    // A fabricated "yesterday" row, inserted directly: `record_call` always
    // keys its own write to `Utc::now()`, so simulating a different day for
    // the rollup means writing that day's row ourselves rather than mocking
    // the clock. This still exercises the real invariant this test cares
    // about — that two distinct `day` rows in `ai_usage` never collapse into
    // one — without giving `record_call` a fake clock it does not have.
    tmp.db
        .write(|c| {
            c.execute(
                "INSERT INTO ai_usage (day, requests, input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens, cost_usd)
                 VALUES ('2020-01-01', 7, 100, 200, 0, 0, 1.5)",
                [],
            )
        })
        .await
        .unwrap();

    record_call(
        &tmp.db,
        CallRecord {
            account_id: None,
            message_id: None,
            request_id: None,
            model: "claude-haiku-4-5".to_owned(),
            pass: None,
            usage: sample_usage(),
            redaction_level: "none".to_owned(),
            latency: Duration::from_millis(10),
            payload: b"x",
            outcome: CallOutcome::Ok,
        },
    )
    .await
    .unwrap();
    // Derive "today" from the row just written rather than a second,
    // independent clock read — see the sibling rollup test for why.
    let written = query_calls(&tmp.db, &AuditFilter::default(), 1, None)
        .await
        .unwrap();
    let today = day_key(written[0].created_at);

    let yesterday_usage = usage_for_day(&tmp.db, "2020-01-01").await.unwrap().unwrap();
    assert_eq!(yesterday_usage.requests, 7);
    assert_eq!(yesterday_usage.input_tokens, 100);

    let today_usage = usage_for_day(&tmp.db, &today).await.unwrap().unwrap();
    assert_eq!(today_usage.requests, 1);
    assert_eq!(
        today_usage.input_tokens,
        i64::from(sample_usage().input_tokens)
    );
}

#[tokio::test]
async fn usage_for_day_is_none_when_nothing_was_recorded() {
    let tmp = TempDb::open();
    assert_eq!(usage_for_day(&tmp.db, "1999-01-01").await.unwrap(), None);
}

// ---------------------------------------------------------------------------
// Querying
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_calls_filters_by_model() {
    let tmp = TempDb::open();
    for model in ["claude-haiku-4-5", "claude-opus-4-8"] {
        record_call(
            &tmp.db,
            CallRecord {
                account_id: None,
                message_id: None,
                request_id: None,
                model: model.to_owned(),
                pass: None,
                usage: sample_usage(),
                redaction_level: "none".to_owned(),
                latency: Duration::from_millis(10),
                payload: b"x",
                outcome: CallOutcome::Ok,
            },
        )
        .await
        .unwrap();
    }

    let filter = AuditFilter {
        model: Some("claude-opus-4-8".to_owned()),
        ..Default::default()
    };
    let entries = query_calls(&tmp.db, &filter, 10, None).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].model, "claude-opus-4-8");
}

#[tokio::test]
async fn query_calls_filters_by_account_id() {
    let tmp = TempDb::open();
    for account_id in [10_i64, 20_i64] {
        record_call(
            &tmp.db,
            CallRecord {
                account_id: Some(account_id),
                message_id: None,
                request_id: None,
                model: "claude-haiku-4-5".to_owned(),
                pass: None,
                usage: sample_usage(),
                redaction_level: "none".to_owned(),
                latency: Duration::from_millis(10),
                payload: b"x",
                outcome: CallOutcome::Ok,
            },
        )
        .await
        .unwrap();
    }

    let filter = AuditFilter {
        account_id: Some(20),
        ..Default::default()
    };
    let entries = query_calls(&tmp.db, &filter, 10, None).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].account_id, Some(20));
}

#[tokio::test]
async fn query_calls_filters_by_message_id() {
    let tmp = TempDb::open();
    for message_id in [100_i64, 200_i64] {
        record_call(
            &tmp.db,
            CallRecord {
                account_id: None,
                message_id: Some(message_id),
                request_id: None,
                model: "claude-haiku-4-5".to_owned(),
                pass: None,
                usage: sample_usage(),
                redaction_level: "none".to_owned(),
                latency: Duration::from_millis(10),
                payload: b"x",
                outcome: CallOutcome::Ok,
            },
        )
        .await
        .unwrap();
    }

    let filter = AuditFilter {
        message_id: Some(200),
        ..Default::default()
    };
    let entries = query_calls(&tmp.db, &filter, 10, None).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].message_id, Some(200));
}

#[tokio::test]
async fn query_calls_filters_by_status() {
    let tmp = TempDb::open();
    record_call(
        &tmp.db,
        CallRecord {
            account_id: None,
            message_id: None,
            request_id: None,
            model: "claude-haiku-4-5".to_owned(),
            pass: None,
            usage: sample_usage(),
            redaction_level: "none".to_owned(),
            latency: Duration::from_millis(10),
            payload: b"x",
            outcome: CallOutcome::Ok,
        },
    )
    .await
    .unwrap();
    record_call(
        &tmp.db,
        CallRecord {
            account_id: None,
            message_id: None,
            request_id: None,
            model: "claude-haiku-4-5".to_owned(),
            pass: None,
            usage: sample_usage(),
            redaction_level: "none".to_owned(),
            latency: Duration::from_millis(10),
            payload: b"x",
            outcome: CallOutcome::Error("boom".to_owned()),
        },
    )
    .await
    .unwrap();

    let ok_filter = AuditFilter {
        status: Some(CallStatus::Ok),
        ..Default::default()
    };
    let ok_entries = query_calls(&tmp.db, &ok_filter, 10, None).await.unwrap();
    assert_eq!(ok_entries.len(), 1);
    assert_eq!(ok_entries[0].status, CallStatus::Ok);

    let error_filter = AuditFilter {
        status: Some(CallStatus::Error),
        ..Default::default()
    };
    let error_entries = query_calls(&tmp.db, &error_filter, 10, None).await.unwrap();
    assert_eq!(error_entries.len(), 1);
    assert_eq!(error_entries[0].status, CallStatus::Error);
}

#[tokio::test]
async fn query_calls_filters_by_time_range() {
    // `record_call` always stamps `created_at` from the real clock, so a
    // since/until test needs rows at known timestamps — seeded directly,
    // bypassing `record_call`, since the append-only triggers (rightly) leave
    // no way to backdate a row after inserting it through the public API.
    let tmp = TempDb::open();
    seed_at(&tmp.db, 1_000, "claude-haiku-4-5").await;
    seed_at(&tmp.db, 2_000, "claude-haiku-4-5").await;
    seed_at(&tmp.db, 3_000, "claude-haiku-4-5").await;

    // A window that straddles the middle row only. A `since`/`until` mixup in
    // the dynamic WHERE-clause builder (e.g. comparing `until` with `>=`)
    // would return the wrong subset here, not merely an empty or full one.
    let filter = AuditFilter {
        since: Some(1_500),
        until: Some(2_500),
        ..Default::default()
    };
    let entries = query_calls(&tmp.db, &filter, 10, None).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].created_at, 2_000);

    // Both bounds are inclusive.
    let inclusive = AuditFilter {
        since: Some(1_000),
        until: Some(3_000),
        ..Default::default()
    };
    let all = query_calls(&tmp.db, &inclusive, 10, None).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn query_calls_paginates_newest_first_with_before_id() {
    let tmp = TempDb::open();
    let mut ids = Vec::new();
    for _ in 0..5 {
        let id = record_call(
            &tmp.db,
            CallRecord {
                account_id: None,
                message_id: None,
                request_id: None,
                model: "claude-haiku-4-5".to_owned(),
                pass: None,
                usage: sample_usage(),
                redaction_level: "none".to_owned(),
                latency: Duration::from_millis(10),
                payload: b"x",
                outcome: CallOutcome::Ok,
            },
        )
        .await
        .unwrap();
        ids.push(id);
    }

    let first_page = query_calls(&tmp.db, &AuditFilter::default(), 2, None)
        .await
        .unwrap();
    assert_eq!(first_page.len(), 2);
    // Newest first: the two most recently inserted ids, descending.
    assert_eq!(first_page[0].id, ids[4]);
    assert_eq!(first_page[1].id, ids[3]);

    let second_page = query_calls(&tmp.db, &AuditFilter::default(), 2, Some(first_page[1].id))
        .await
        .unwrap();
    assert_eq!(second_page.len(), 2);
    assert_eq!(second_page[0].id, ids[2]);
    assert_eq!(second_page[1].id, ids[1]);
    // No overlap between pages.
    assert!(!second_page
        .iter()
        .any(|e| e.id == first_page[0].id || e.id == first_page[1].id));
}

// `ExportLedger`'s bulk-export behavior (paging past a single `query_calls`
// batch without dropping or duplicating a row at the boundary) is exercised
// in `rmaild/tests/audit_service.rs` — that is where the paging loop actually
// lives (`rmaild::audit_service::export_ledger`); this module only owns the
// single-page primitive it is built from.

// ---------------------------------------------------------------------------
// Cost estimation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cost_is_persisted_using_the_same_estimate_the_pricing_table_produces() {
    let tmp = TempDb::open();
    let usage = sample_usage();
    record_call(
        &tmp.db,
        CallRecord {
            account_id: None,
            message_id: None,
            request_id: None,
            model: "claude-sonnet-5".to_owned(),
            pass: None,
            usage,
            redaction_level: "none".to_owned(),
            latency: Duration::from_millis(10),
            payload: b"x",
            outcome: CallOutcome::Ok,
        },
    )
    .await
    .unwrap();

    let entries = query_calls(&tmp.db, &AuditFilter::default(), 10, None)
        .await
        .unwrap();
    let expected = estimate_cost_usd("claude-sonnet-5", usage);
    assert!((entries[0].cost_usd - expected).abs() < 1e-12);
}

#[test]
fn estimate_cost_usd_matches_published_per_token_pricing() {
    // One million of each kind of token, so each priced component lands on a
    // whole-dollar figure computed independently of `pricing_for`'s internals
    // — a regression that scrambled the four multipliers would still get the
    // total number of tokens right but not this total.
    let usage = Usage {
        input_tokens: 1_000_000,
        output_tokens: 1_000_000,
        cache_creation_input_tokens: 1_000_000,
        cache_read_input_tokens: 1_000_000,
    };

    // claude-haiku-4-5: $1.00 in / $5.00 out per MTok; cache write 2x input,
    // cache read 0.1x input.
    let haiku = estimate_cost_usd("claude-haiku-4-5", usage);
    assert!(
        (haiku - (1.00 + 5.00 + 2.00 + 0.10)).abs() < 1e-9,
        "{haiku}"
    );

    // claude-sonnet-5: $3.00 / $15.00 per MTok (standard rate, not the
    // introductory discount — see `pricing_for`'s doc comment).
    let sonnet = estimate_cost_usd("claude-sonnet-5", usage);
    assert!(
        (sonnet - (3.00 + 15.00 + 6.00 + 0.30)).abs() < 1e-9,
        "{sonnet}"
    );

    // claude-opus-4-8: $5.00 / $25.00 per MTok.
    let opus = estimate_cost_usd("claude-opus-4-8", usage);
    assert!(
        (opus - (5.00 + 25.00 + 10.00 + 0.50)).abs() < 1e-9,
        "{opus}"
    );
}

#[test]
fn estimate_cost_usd_is_zero_for_an_unrecognized_model() {
    assert_eq!(
        estimate_cost_usd("some-future-model-not-in-the-table", sample_usage()),
        0.0
    );
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

#[test]
fn day_key_formats_the_utc_calendar_day() {
    let ts = chrono::Utc
        .with_ymd_and_hms(2026, 8, 5, 23, 59, 59)
        .unwrap()
        .timestamp();
    assert_eq!(day_key(ts), "2026-08-05");
}

#[test]
fn call_status_round_trips_through_its_wire_string() {
    for status in [CallStatus::Ok, CallStatus::Error] {
        assert_eq!(CallStatus::parse(status.as_str()).unwrap(), status);
    }
    assert!(CallStatus::parse("bogus").is_err());
}
