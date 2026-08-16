//! Response-time analytics: percentile math, In-Reply-To/References pairing,
//! direction classification, the bottleneck flag, the rolling trend, and the
//! degenerate shapes analytics is easy to get wrong on — empty ranges,
//! single-message threads, threads nobody answered, and every place a naive
//! implementation would divide by zero.
#![allow(clippy::panic)] // `group()` names the missing key in its failure

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::repo;
use crate::thread::assign_thread;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 2023-11-14T22:13:20Z — an arbitrary but fixed epoch, so every assertion
/// below is about a difference and never about "now".
const T0: i64 = 1_700_000_000;

const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

const ME: &str = "me@example.com";
const ALIAS: &str = "me+lists@example.com";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A message described only by the fields this module reads.
#[derive(Default, Clone)]
struct Msg<'a> {
    message_id: Option<&'a str>,
    in_reply_to: Option<&'a str>,
    references: Option<&'a str>,
    subject: Option<&'a str>,
    from: Option<&'a str>,
    from_name: Option<&'a str>,
    at: Option<i64>,
    mailbox: Option<i64>,
}

impl<'a> Msg<'a> {
    fn new(id: &'a str, from: &'a str, at: i64) -> Self {
        Self {
            message_id: Some(id),
            from: Some(from),
            at: Some(at),
            subject: Some("Project"),
            ..Default::default()
        }
    }
    fn reply_to(mut self, parent: &'a str) -> Self {
        self.in_reply_to = Some(parent);
        self
    }
    fn refs(mut self, refs: &'a str) -> Self {
        self.references = Some(refs);
        self
    }
    fn subject(mut self, subject: &'a str) -> Self {
        self.subject = Some(subject);
        self
    }
    fn name(mut self, name: &'a str) -> Self {
        self.from_name = Some(name);
        self
    }
    fn in_mailbox(mut self, mailbox: i64) -> Self {
        self.mailbox = Some(mailbox);
        self
    }
}

struct Fx {
    db: Database,
    path: PathBuf,
    account_id: i64,
    inbox: i64,
    sent: i64,
    next_uid: Cell<i64>,
}

impl Fx {
    /// An account whose configured username is [`ME`], with an `INBOX` and a
    /// `Sent` folder.
    fn open() -> Self {
        Self::open_with_username(Some(ME))
    }

    fn open_with_username(username: Option<&str>) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-rt-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        let (account_id, inbox, sent) = db
            .with_write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: format!("Personal-{n}"),
                        username: username.map(str::to_owned),
                        ..Default::default()
                    },
                )?;
                let inbox = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                let sent = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "Sent".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox, sent))
            })
            .unwrap();
        Self {
            db,
            path,
            account_id,
            inbox,
            sent,
            next_uid: Cell::new(1),
        }
    }

    /// A second account with its own `INBOX`, returning `(account, inbox)`.
    fn add_account(&self, name: &str, username: &str) -> (i64, i64) {
        self.db
            .with_write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: name.to_owned(),
                        username: Some(username.to_owned()),
                        ..Default::default()
                    },
                )?;
                let inbox = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox))
            })
            .unwrap()
    }

    fn add_mailbox(&self, name: &str) -> i64 {
        self.db
            .with_write(|c| {
                repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id: self.account_id,
                        name: name.to_owned(),
                        ..Default::default()
                    },
                )
            })
            .unwrap()
    }

    /// Insert a message and thread it. Mail from a self address defaults to
    /// `Sent`, everything else to `INBOX` — the arrangement a real mailbox
    /// has, and the one the Sent-folder identity probe reads.
    fn add(&self, msg: Msg<'_>) -> i64 {
        let outbound = msg
            .from
            .is_some_and(|from| from.eq_ignore_ascii_case(ME) || from.eq_ignore_ascii_case(ALIAS));
        let mailbox_id = msg
            .mailbox
            .unwrap_or(if outbound { self.sent } else { self.inbox });
        self.add_in(self.account_id, mailbox_id, msg)
    }

    /// Insert into a specific account and folder, ignoring `Msg::in_mailbox`.
    fn add_in(&self, account_id: i64, mailbox_id: i64, msg: Msg<'_>) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            message_id: msg.message_id.map(str::to_owned),
            in_reply_to: msg.in_reply_to.map(str::to_owned),
            references_hdr: msg.references.map(str::to_owned),
            subject: msg.subject.map(str::to_owned),
            from_addr: msg.from.map(str::to_owned),
            from_name: msg.from_name.map(str::to_owned),
            date: msg.at,
            ..Default::default()
        };
        self.db
            .with_write(|c| {
                let id = repo::insert_message(c, &new)?;
                assign_thread(c, id)?;
                Ok(id)
            })
            .unwrap()
    }

    /// A query over `[T0, T0 + 30 days)` with weekly buckets.
    fn query(&self) -> ResponseTimeQuery {
        ResponseTimeQuery {
            account_id: Some(self.account_id),
            group_by: GroupBy::Contact,
            since: T0,
            until: T0 + 30 * DAY,
            bucket_seconds: 7 * DAY,
            window_seconds: 28 * DAY,
            limit: DEFAULT_LIMIT,
            min_samples: DEFAULT_MIN_SAMPLES,
            bottleneck_ratio: DEFAULT_BOTTLENECK_RATIO,
        }
    }

    async fn run(&self, query: ResponseTimeQuery) -> ResponseTimes {
        response_times(&self.db, &CancellationToken::new(), query)
            .await
            .unwrap()
    }

    async fn run_err(&self, query: ResponseTimeQuery) -> Error {
        response_times(&self.db, &CancellationToken::new(), query)
            .await
            .unwrap_err()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// Find a group by key, failing loudly rather than silently comparing against
/// a default.
fn group<'a>(report: &'a ResponseTimes, key: &str) -> &'a ResponseGroup {
    report
        .groups
        .iter()
        .find(|g| g.key == key)
        .unwrap_or_else(|| {
            panic!(
                "no group {key:?}; report has {:?}",
                report.groups.iter().map(|g| &g.key).collect::<Vec<_>>()
            )
        })
}

/// A two-message exchange: they write, we answer `after` seconds later.
fn exchange(fx: &Fx, tag: &str, them: &str, at: i64, after: i64) {
    fx.add(Msg::new(&format!("{tag}-in@x"), them, at).subject(tag));
    fx.add(
        Msg::new(&format!("{tag}-out@x"), ME, at + after)
            .subject(tag)
            .reply_to(&format!("{tag}-in@x"))
            .refs(&format!("{tag}-in@x")),
    );
}

// ---------------------------------------------------------------------------
// Percentile math
// ---------------------------------------------------------------------------

#[test]
fn percentile_of_nothing_is_undefined_not_zero() {
    assert_eq!(percentile(&[], 50), None);
    assert_eq!(percentile(&[], 90), None);
}

#[test]
fn percentile_uses_nearest_rank() {
    // Ranks are 1-based ceil(p/100 * n), so with n = 10 the p50 is the 5th
    // value and the p90 the 9th — both real observations, never a midpoint.
    let sorted: Vec<i64> = (1..=10).collect();
    assert_eq!(percentile(&sorted, 50), Some(5));
    assert_eq!(percentile(&sorted, 90), Some(9));
    assert_eq!(percentile(&sorted, 100), Some(10));
    assert_eq!(percentile(&sorted, 0), Some(1));
}

#[test]
fn percentile_of_one_sample_is_that_sample() {
    assert_eq!(percentile(&[42], 50), Some(42));
    assert_eq!(percentile(&[42], 90), Some(42));
}

#[test]
fn percentile_never_interpolates_between_two_neighbours() {
    // The interpolating definition of p50 over [10, 20] is 15, which is not
    // an observation. Nearest rank picks the lower of the two.
    assert_eq!(percentile(&[10, 20], 50), Some(10));
    assert_eq!(percentile(&[10, 20], 90), Some(20));
}

#[test]
fn stats_of_nothing_report_zero_samples() {
    let stats = Stats::from_sorted(&[]);
    assert_eq!(stats.samples, 0);
    assert_eq!(stats.p50_seconds, 0);
    assert_eq!(stats.p90_seconds, 0);
    assert!(stats.mean_seconds.abs() < f64::EPSILON, "no NaN from 0/0");
}

#[test]
fn stats_report_mean_min_and_max() {
    let stats = Stats::from_sorted(&[1, 2, 3, 10]);
    assert_eq!(stats.samples, 4);
    assert_eq!(stats.min_seconds, 1);
    assert_eq!(stats.max_seconds, 10);
    assert!((stats.mean_seconds - 4.0).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_inverted_window_is_invalid_argument() {
    let fx = Fx::open();
    let mut query = fx.query();
    query.until = query.since;
    assert_eq!(
        fx.run_err(query).await.reason(),
        ErrorReason::InvalidArgument
    );
}

#[tokio::test]
async fn a_non_positive_bucket_is_invalid_argument() {
    let fx = Fx::open();
    let mut query = fx.query();
    query.bucket_seconds = 0;
    assert_eq!(
        fx.run_err(query).await.reason(),
        ErrorReason::InvalidArgument
    );
}

#[tokio::test]
async fn a_rolling_window_shorter_than_its_step_is_invalid_argument() {
    let fx = Fx::open();
    let mut query = fx.query();
    query.window_seconds = query.bucket_seconds - 1;
    assert_eq!(
        fx.run_err(query).await.reason(),
        ErrorReason::InvalidArgument
    );
}

#[tokio::test]
async fn a_bottleneck_ratio_below_one_is_invalid_argument() {
    let fx = Fx::open();
    for ratio in [0.5, f64::NAN, f64::NEG_INFINITY] {
        let mut query = fx.query();
        query.bottleneck_ratio = ratio;
        assert_eq!(
            fx.run_err(query).await.reason(),
            ErrorReason::InvalidArgument,
            "ratio {ratio} must be rejected"
        );
    }
}

#[tokio::test]
async fn too_fine_a_bucket_is_rejected_rather_than_truncated() {
    let fx = Fx::open();
    let mut query = fx.query();
    query.since = T0;
    query.until = T0 + 5 * 365 * DAY;
    query.bucket_seconds = DAY;
    let error = fx.run_err(query).await;
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(
        error.to_string().contains("bucket_seconds"),
        "the message must say which knob to turn: {error}"
    );
}

#[tokio::test]
async fn the_group_limit_is_clamped_rather_than_rejected() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    let mut query = fx.query();
    query.limit = usize::MAX;
    let report = fx.run(query).await;
    assert_eq!(report.groups.len(), 1);
}

/// The clamps themselves, asserted where they happen — a report with one
/// group cannot tell `truncate(usize::MAX)` from `truncate(MAX_LIMIT)`.
#[test]
fn validation_clamps_the_limit_and_floors_the_evidence_bar() {
    let base = ResponseTimeQuery::ending_at(T0);

    let mut huge = ResponseTimeQuery {
        limit: usize::MAX,
        min_samples: 0,
        ..base.clone()
    };
    huge.validate().unwrap();
    assert_eq!(huge.limit, MAX_LIMIT);
    assert_eq!(huge.min_samples, 1, "a bar of zero would flag everything");

    let mut tiny = ResponseTimeQuery {
        limit: 0,
        ..base.clone()
    };
    tiny.validate().unwrap();
    assert_eq!(tiny.limit, 1, "0 groups would make every report empty");

    let mut kept = ResponseTimeQuery {
        limit: 7,
        min_samples: 9,
        ..base
    };
    kept.validate().unwrap();
    assert_eq!(kept.limit, 7);
    assert_eq!(kept.min_samples, 9);
}

#[tokio::test]
async fn a_cancelled_scan_is_an_error_not_an_empty_report() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = response_times(&fx.db, &cancel, fx.query())
        .await
        .unwrap_err();
    assert_eq!(
        error.reason(),
        ErrorReason::Cancelled,
        "a half-read mailbox must never be summarized as a whole one"
    );
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pairs_a_sent_reply_to_the_message_it_answers() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, 4 * HOUR);

    let report = fx.run(fx.query()).await;
    assert_eq!(report.pairs, 1);
    assert_eq!(report.ours.samples, 1);
    assert_eq!(report.ours.p50_seconds, 4 * HOUR);
    assert_eq!(report.ours.p90_seconds, 4 * HOUR);
    assert_eq!(report.theirs.samples, 0);
    assert_eq!(group(&report, "alice@x").ours.p50_seconds, 4 * HOUR);
}

#[tokio::test]
async fn falls_back_to_the_last_references_id_when_in_reply_to_is_absent() {
    let fx = Fx::open();
    fx.add(Msg::new("root@x", "alice@x", T0 + DAY));
    fx.add(
        Msg::new("mid@x", ME, T0 + DAY + HOUR)
            .reply_to("root@x")
            .refs("root@x"),
    );
    fx.add(Msg::new("leaf@x", "alice@x", T0 + DAY + 2 * HOUR).refs("root@x mid@x"));

    let report = fx.run(fx.query()).await;
    // The leaf names no In-Reply-To, so it must attach to `mid@x` — the last
    // References entry — and count as *their* one-hour reply to us, not as a
    // two-hour reply to the root.
    assert_eq!(report.theirs.samples, 1);
    assert_eq!(report.theirs.p50_seconds, HOUR);
}

#[tokio::test]
async fn an_angle_bracketed_header_still_matches_a_bare_message_id() {
    let fx = Fx::open();
    fx.add(Msg::new("root@x", "alice@x", T0 + DAY));
    fx.add(Msg::new("out@x", ME, T0 + DAY + HOUR).reply_to("<root@x>"));

    let report = fx.run(fx.query()).await;
    assert_eq!(
        report.ours.samples, 1,
        "brackets must be stripped on both sides"
    );
}

#[tokio::test]
async fn a_single_message_thread_produces_no_pairs() {
    let fx = Fx::open();
    fx.add(Msg::new("lonely@x", "alice@x", T0 + DAY));

    let report = fx.run(fx.query()).await;
    assert_eq!(report.pairs, 0);
    assert_eq!(report.ours.samples, 0);
    assert_eq!(report.theirs.samples, 0);
    assert!(
        report.groups.is_empty(),
        "one message is not a correspondence"
    );
    assert_eq!(report.total_groups, 0);
}

#[tokio::test]
async fn a_reply_to_a_message_that_was_never_synced_is_dropped() {
    let fx = Fx::open();
    // The parent is only referenced, never fetched — the "phantom" case
    // threading is built to survive. There is no timestamp to subtract from,
    // so there is no latency to report.
    fx.add(Msg::new("out@x", ME, T0 + DAY).reply_to("never-seen@x"));

    let report = fx.run(fx.query()).await;
    assert_eq!(report.pairs, 0);
    assert_eq!(report.skipped_out_of_order, 0);
}

#[tokio::test]
async fn references_is_a_fallback_for_a_missing_header_not_a_failed_lookup() {
    let fx = Fx::open();
    // The thread root is synced — and, because we answered it directly, it is
    // loaded and sitting in the originals map, so a chain-walking fallback
    // really would find it. The message the second reply actually answers is
    // not synced. Re-aiming that reply at the root would invent a two-day
    // response we never made, and invent it as *slower* than the truth: the
    // one direction a report about your own slowness must never guess in.
    fx.add(Msg::new("root@x", "alice@x", T0 + DAY));
    fx.add(Msg::new("prompt@x", ME, T0 + DAY + HOUR).reply_to("root@x"));
    fx.add(
        Msg::new("late@x", ME, T0 + 3 * DAY)
            .reply_to("unsynced@x")
            .refs("root@x unsynced@x"),
    );

    let report = fx.run(fx.query()).await;
    assert_eq!(
        report.pairs, 1,
        "only the reply whose parent is known counts"
    );
    assert_eq!(report.ours.samples, 1);
    assert_eq!(report.ours.max_seconds, HOUR, "no invented two-day latency");
}

#[tokio::test]
async fn their_reply_to_us_is_counted_on_their_side() {
    let fx = Fx::open();
    fx.add(Msg::new("out@x", ME, T0 + DAY));
    fx.add(Msg::new("in@x", "alice@x", T0 + DAY + 3 * HOUR).reply_to("out@x"));

    let report = fx.run(fx.query()).await;
    assert_eq!(report.ours.samples, 0);
    assert_eq!(report.theirs.samples, 1);
    assert_eq!(report.theirs.p50_seconds, 3 * HOUR);
    let alice = group(&report, "alice@x");
    assert_eq!(alice.theirs.p50_seconds, 3 * HOUR);
    assert_eq!(alice.ours.samples, 0);
}

#[tokio::test]
async fn a_note_to_self_is_not_a_response_time() {
    let fx = Fx::open();
    fx.add(Msg::new("a@x", ME, T0 + DAY));
    fx.add(Msg::new("b@x", ALIAS, T0 + DAY + HOUR).reply_to("a@x"));
    // The alias only becomes an identity once something in Sent was sent from
    // it, which the message above is.
    let report = fx.run(fx.query()).await;
    assert!(
        report.self_addresses.contains(&ALIAS.to_owned()),
        "the alias must be recognized: {:?}",
        report.self_addresses
    );
    assert_eq!(report.pairs, 0, "you replying to yourself is not a latency");
}

#[tokio::test]
async fn two_other_people_talking_on_a_list_are_not_counted() {
    let fx = Fx::open();
    fx.add(Msg::new("a@x", "alice@x", T0 + DAY));
    fx.add(Msg::new("b@x", "bob@x", T0 + DAY + HOUR).reply_to("a@x"));

    let report = fx.run(fx.query()).await;
    assert_eq!(report.pairs, 0);
    assert!(report.groups.is_empty());
}

#[tokio::test]
async fn a_reply_that_predates_what_it_answers_is_skipped_and_counted() {
    let fx = Fx::open();
    fx.add(Msg::new("in@x", "alice@x", T0 + 2 * DAY));
    // A forged or skewed Date header: the "reply" is stamped a day earlier.
    fx.add(Msg::new("out@x", ME, T0 + DAY).reply_to("in@x"));

    let report = fx.run(fx.query()).await;
    assert_eq!(
        report.pairs, 0,
        "a negative latency must not become a sample"
    );
    assert_eq!(report.skipped_out_of_order, 1);
    assert_eq!(report.ours.samples, 0);
}

#[tokio::test]
async fn a_timestamp_that_would_overflow_the_subtraction_is_skipped() {
    let fx = Fx::open();
    // `Date:` is attacker-controlled. `responded_at - i64::MIN` overflows,
    // which panics in a debug build and wraps to a plausible-looking latency
    // in a release one.
    fx.add(Msg::new("in@x", "alice@x", i64::MIN));
    fx.add(Msg::new("out@x", ME, T0 + DAY).reply_to("in@x"));

    let report = fx.run(fx.query()).await;
    assert_eq!(report.pairs, 0);
    assert_eq!(report.skipped_out_of_order, 1);
}

/// The scan ceiling, asserted where it is decided — a test that actually put
/// [`MAX_SCAN_ROWS`] messages in a database would cost minutes to prove one
/// comparison.
#[test]
fn a_scan_past_the_row_ceiling_is_refused_rather_than_truncated() {
    let under: Vec<i64> = vec![0; MAX_SCAN_ROWS];
    assert_eq!(within_cap(under, "reply").unwrap().len(), MAX_SCAN_ROWS);

    let over: Vec<i64> = vec![0; MAX_SCAN_ROWS + 1];
    let error = within_cap(over, "reply").unwrap_err();
    assert_eq!(error.reason(), ErrorReason::ResourceExhausted);
    assert!(
        error.to_string().contains("narrow"),
        "the message must say what to do about it: {error}"
    );

    assert_eq!(
        scan_limit(),
        i64::try_from(MAX_SCAN_ROWS).unwrap() + 1,
        "the statements must fetch one row past the cap, or nothing ever trips it"
    );
}

#[tokio::test]
async fn the_same_reply_filed_in_two_folders_counts_once() {
    let fx = Fx::open();
    let archive = fx.add_mailbox("Archive");
    fx.add(Msg::new("in@x", "alice@x", T0 + DAY));
    fx.add(Msg::new("out@x", ME, T0 + DAY + HOUR).reply_to("in@x"));
    // The same Message-ID, filed again after an archive/copy.
    fx.add(
        Msg::new("out@x", ME, T0 + DAY + HOUR)
            .reply_to("in@x")
            .in_mailbox(archive),
    );

    let report = fx.run(fx.query()).await;
    assert_eq!(report.pairs, 1, "one message filed twice is one response");
    assert_eq!(report.ours.samples, 1);
}

#[tokio::test]
async fn the_same_inbound_message_filed_in_two_folders_counts_once() {
    let fx = Fx::open();
    let archive = fx.add_mailbox("Archive");
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    // A second unanswered message from her, filed twice.
    fx.add(Msg::new("dup@x", "alice@x", T0 + 2 * DAY).subject("Q"));
    fx.add(
        Msg::new("dup@x", "alice@x", T0 + 2 * DAY)
            .subject("Q")
            .in_mailbox(archive),
    );

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.inbound, 2, "one message filed twice is one message");
    assert_eq!(alice.awaiting_reply, 1);
}

#[tokio::test]
async fn a_reply_outside_the_window_is_not_in_the_report() {
    let fx = Fx::open();
    exchange(&fx, "inside", "alice@x", T0 + DAY, HOUR);
    exchange(&fx, "after", "alice@x", T0 + 40 * DAY, HOUR);
    exchange(&fx, "before", "alice@x", T0 - 40 * DAY, HOUR);

    let report = fx.run(fx.query()).await;
    assert_eq!(report.ours.samples, 1, "the window is on the reply");
}

#[tokio::test]
async fn an_old_message_answered_inside_the_window_is_counted_in_full() {
    let fx = Fx::open();
    // Arrived long before `since`; answered on day two. The latency is the
    // real one, not the part of it that fell inside the window.
    fx.add(Msg::new("in@x", "alice@x", T0 - 10 * DAY));
    fx.add(Msg::new("out@x", ME, T0 + 2 * DAY).reply_to("in@x"));

    let report = fx.run(fx.query()).await;
    assert_eq!(report.ours.samples, 1);
    assert_eq!(report.ours.p50_seconds, 12 * DAY);
}

#[tokio::test]
async fn an_empty_range_reports_zero_everywhere_without_dividing_by_zero() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);

    let mut query = fx.query();
    query.since = T0 + 100 * DAY;
    query.until = T0 + 130 * DAY;
    let report = fx.run(query).await;

    assert_eq!(report.pairs, 0);
    assert_eq!(report.ours, Stats::default());
    assert_eq!(report.theirs, Stats::default());
    assert!(report.groups.is_empty());
    assert!(!report.trend.is_empty(), "the trend still spans the range");
    for point in &report.trend {
        assert_eq!(point.stats.samples, 0);
        assert!(point.stats.mean_seconds.abs() < f64::EPSILON);
    }
}

// ---------------------------------------------------------------------------
// Grouping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn groups_by_contact_with_per_contact_percentiles() {
    let fx = Fx::open();
    for (i, after) in [HOUR, 2 * HOUR, 3 * HOUR, 4 * HOUR, 100 * HOUR]
        .into_iter()
        .enumerate()
    {
        exchange(
            &fx,
            &format!("al{i}"),
            "alice@x",
            T0 + DAY + i as i64 * DAY,
            after,
        );
    }
    exchange(&fx, "bo", "bob@x", T0 + DAY, 10 * DAY);

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.ours.samples, 5);
    // Nearest rank over [1h, 2h, 3h, 4h, 100h]: p50 is the 3rd, p90 the 5th.
    assert_eq!(alice.ours.p50_seconds, 3 * HOUR);
    assert_eq!(alice.ours.p90_seconds, 100 * HOUR);
    assert_eq!(group(&report, "bob@x").ours.p50_seconds, 10 * DAY);
    assert_eq!(report.total_groups, 2);
}

#[tokio::test]
async fn the_contact_label_prefers_the_most_recent_display_name() {
    let fx = Fx::open();
    fx.add(Msg::new("in1@x", "alice@x", T0 + DAY).name("A. Anderson"));
    fx.add(Msg::new("out1@x", ME, T0 + DAY + HOUR).reply_to("in1@x"));
    fx.add(Msg::new("in2@x", "alice@x", T0 + 5 * DAY).name("Alice Anderson"));
    fx.add(Msg::new("out2@x", ME, T0 + 5 * DAY + HOUR).reply_to("in2@x"));

    let report = fx.run(fx.query()).await;
    assert_eq!(group(&report, "alice@x").label, "Alice Anderson");
}

#[tokio::test]
async fn the_label_is_dated_by_the_message_it_came_off_not_by_our_reply() {
    let fx = Fx::open();
    // The old name arrives first but is answered *last*; the new name arrives
    // later and is answered immediately. Keying label recency on our own
    // reply time therefore picks the stale one — the two orderings disagree
    // by construction, which is the only way to tell the two clocks apart.
    fx.add(Msg::new("in1@x", "alice@x", T0 + DAY).name("A. Anderson"));
    fx.add(Msg::new("in2@x", "alice@x", T0 + 5 * DAY).name("Alice Anderson"));
    fx.add(Msg::new("out2@x", ME, T0 + 5 * DAY + HOUR).reply_to("in2@x"));
    fx.add(Msg::new("out1@x", ME, T0 + 9 * DAY).reply_to("in1@x"));

    let report = fx.run(fx.query()).await;
    assert_eq!(group(&report, "alice@x").label, "Alice Anderson");
}

#[tokio::test]
async fn a_contact_with_no_display_name_falls_back_to_the_address() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    let report = fx.run(fx.query()).await;
    assert_eq!(group(&report, "alice@x").label, "alice@x");
}

#[tokio::test]
async fn groups_by_mailbox_key_on_the_folder_the_inbound_mail_is_in() {
    let fx = Fx::open();
    let project = fx.add_mailbox("Projects");
    fx.add(Msg::new("p-in@x", "alice@x", T0 + DAY).in_mailbox(project));
    fx.add(Msg::new("p-out@x", ME, T0 + DAY + 5 * DAY).reply_to("p-in@x"));
    exchange(&fx, "i", "bob@x", T0 + DAY, HOUR);

    let mut query = fx.query();
    query.group_by = GroupBy::Mailbox;
    let report = fx.run(query).await;

    assert_eq!(report.group_by, GroupBy::Mailbox);
    let projects = group(&report, "Projects");
    assert_eq!(projects.mailbox_id, Some(project));
    assert_eq!(projects.ours.p50_seconds, 5 * DAY);
    assert_eq!(group(&report, "INBOX").ours.p50_seconds, HOUR);
    assert!(
        report.groups.iter().all(|g| g.key != "Sent"),
        "a pair is keyed on the side that was waited for, never on Sent"
    );
}

#[tokio::test]
async fn a_folder_full_of_mail_from_strangers_is_not_a_bottleneck() {
    let fx = Fx::open();
    // One real correspondence in INBOX, answered promptly...
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    // ...alongside twenty ancient newsletters nobody replies to. Under
    // mailbox grouping they all land in the INBOX group, so a "does this
    // group contain anyone you answer" test would flag every folder in every
    // real mailbox.
    for i in 0..20 {
        fx.add(Msg::new(&format!("n{i}@x"), &format!("news{i}@x"), T0 + 2 * DAY).subject("N"));
    }

    let mut query = fx.query();
    query.group_by = GroupBy::Mailbox;
    let report = fx.run(query).await;
    let inbox = group(&report, "INBOX");
    assert_eq!(inbox.inbound, 21);
    assert_eq!(inbox.awaiting_reply, 20);
    assert_eq!(
        inbox.overdue, 0,
        "overdue is per-sender: none of these senders is one you answer"
    );
    assert!(!inbox.stalled);
    assert!(!inbox.bottleneck);
}

#[tokio::test]
async fn mail_from_someone_you_do_answer_still_stalls_its_folder() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    for i in 0..3 {
        fx.add(Msg::new(&format!("drop{i}@x"), "alice@x", T0 + (2 + i) * DAY).subject("Q"));
    }
    // Noise from strangers in the same folder must neither add to nor mask
    // the real signal.
    for i in 0..10 {
        fx.add(Msg::new(&format!("n{i}@x"), &format!("news{i}@x"), T0 + 2 * DAY).subject("N"));
    }

    let mut query = fx.query();
    query.group_by = GroupBy::Mailbox;
    let report = fx.run(query).await;
    let inbox = group(&report, "INBOX");
    assert_eq!(inbox.awaiting_reply, 13);
    assert_eq!(inbox.overdue, 3);
    assert!(inbox.stalled);
}

#[tokio::test]
async fn two_accounts_identically_named_folders_stay_separate_groups() {
    let fx = Fx::open();
    let (other_account, other_inbox) = fx.add_account("Work", "me@work.example");
    fx.add_in(
        other_account,
        other_inbox,
        Msg::new("w-in@x", "carol@x", T0 + DAY),
    );
    fx.add_in(
        other_account,
        other_inbox,
        Msg::new("w-out@x", "me@work.example", T0 + DAY + 5 * DAY).reply_to("w-in@x"),
    );
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);

    let mut query = fx.query();
    query.account_id = None;
    query.group_by = GroupBy::Mailbox;
    let report = fx.run(query).await;

    let inboxes: Vec<&ResponseGroup> = report.groups.iter().filter(|g| g.key == "INBOX").collect();
    assert_eq!(
        inboxes.len(),
        2,
        "one median over two unrelated mailboxes would be meaningless: {:?}",
        report.groups
    );
    let mut medians: Vec<i64> = inboxes.iter().map(|g| g.ours.p50_seconds).collect();
    medians.sort_unstable();
    assert_eq!(medians, vec![HOUR, 5 * DAY]);
    let mut ids: Vec<Option<i64>> = inboxes.iter().map(|g| g.mailbox_id).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![Some(fx.inbox), Some(other_inbox)],
        "each group must name the folder it is actually about"
    );
}

#[tokio::test]
async fn the_group_limit_truncates_but_reports_the_full_count() {
    let fx = Fx::open();
    for i in 0..5 {
        exchange(
            &fx,
            &format!("c{i}"),
            &format!("contact{i}@x"),
            T0 + DAY,
            (i as i64 + 1) * HOUR,
        );
    }
    let mut query = fx.query();
    query.limit = 2;
    let report = fx.run(query).await;

    assert_eq!(report.groups.len(), 2);
    assert_eq!(report.total_groups, 5);
    // Slowest first.
    assert_eq!(report.groups[0].key, "contact4@x");
    assert_eq!(report.groups[1].key, "contact3@x");
}

// ---------------------------------------------------------------------------
// Bottleneck flagging
// ---------------------------------------------------------------------------

/// `n` pairs of threads with Alice: one she opens and we answer after `ours`,
/// one we open, she answers after `theirs`, and we acknowledge after `ours`
/// again.
///
/// The acknowledgement is not decoration. Without it every second thread
/// would end on her message and the `overdue` arm of the bottleneck rule
/// would fire on both the fast and the slow fixture, hiding whichever
/// behavior the test was actually about.
fn symmetric_exchange(fx: &Fx, n: i64, ours: i64, theirs: i64) {
    for i in 0..n {
        let base = T0 + (i + 1) * DAY;
        let tag = format!("t{i}");
        // They open; we answer after `ours`.
        fx.add(Msg::new(&format!("{tag}-in@x"), "alice@x", base).subject(&tag));
        fx.add(
            Msg::new(&format!("{tag}-out@x"), ME, base + ours)
                .subject(&tag)
                .reply_to(&format!("{tag}-in@x")),
        );
        // We open a second thread; they answer after `theirs`; we close it
        // after `ours`.
        let tag2 = format!("u{i}");
        fx.add(Msg::new(&format!("{tag2}-out@x"), ME, base).subject(&tag2));
        fx.add(
            Msg::new(&format!("{tag2}-in@x"), "alice@x", base + theirs)
                .subject(&tag2)
                .reply_to(&format!("{tag2}-out@x")),
        );
        fx.add(
            Msg::new(&format!("{tag2}-ack@x"), ME, base + theirs + ours)
                .subject(&tag2)
                .reply_to(&format!("{tag2}-in@x")),
        );
    }
}

#[tokio::test]
async fn the_user_is_flagged_when_their_median_is_multiples_of_the_counterparts() {
    let fx = Fx::open();
    symmetric_exchange(&fx, 4, 10 * HOUR, HOUR);

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.ours.p50_seconds, 10 * HOUR);
    assert_eq!(alice.theirs.p50_seconds, HOUR);
    assert!(alice.slower_than_counterpart);
    assert!(alice.bottleneck);
    assert_eq!(alice.overdue, 0, "nothing is left dangling in this fixture");
    assert_eq!(
        alice.bottleneck,
        alice.slower_than_counterpart || alice.stalled
    );
}

#[tokio::test]
async fn the_user_is_not_flagged_when_they_are_the_faster_side() {
    let fx = Fx::open();
    symmetric_exchange(&fx, 4, HOUR, 10 * HOUR);

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.ours.p50_seconds, HOUR);
    assert_eq!(alice.theirs.p50_seconds, 10 * HOUR);
    assert!(!alice.slower_than_counterpart);
    assert!(!alice.stalled);
    assert!(!alice.bottleneck);
}

#[tokio::test]
async fn one_slow_reply_does_not_clear_the_evidence_bar() {
    let fx = Fx::open();
    // A single 10-hour reply against a single 1-hour one: the ratio is met,
    // the sample count is not.
    symmetric_exchange(&fx, 1, 10 * HOUR, HOUR);

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.ours.samples, 2);
    assert_eq!(alice.theirs.samples, 1);
    assert!(!alice.slower_than_counterpart, "min_samples is 3");
    assert!(!alice.bottleneck);
}

#[tokio::test]
async fn a_counterparty_who_answers_instantly_does_not_divide_by_zero() {
    let fx = Fx::open();
    // Their median latency is exactly zero — an auto-responder. The rule has
    // to survive it, and has to still flag us.
    symmetric_exchange(&fx, 4, 6 * HOUR, 0);

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.theirs.p50_seconds, 0);
    assert!(alice.ours.p50_seconds > 0);
    assert!(
        alice.slower_than_counterpart,
        "a zero denominator must not swallow the comparison"
    );
}

#[tokio::test]
async fn instant_on_both_sides_is_not_a_bottleneck() {
    let fx = Fx::open();
    symmetric_exchange(&fx, 4, 0, 0);

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.ours.p50_seconds, 0);
    assert_eq!(alice.theirs.p50_seconds, 0);
    assert!(
        !alice.slower_than_counterpart,
        "0 >= 0 * ratio is true, and flagging on it would be exactly backwards"
    );
    assert!(!alice.bottleneck);
}

#[tokio::test]
async fn the_rule_needs_evidence_on_both_sides() {
    let fx = Fx::open();
    // Four slow replies from us, none from them: nothing to be slower *than*.
    for i in 0..4 {
        exchange(
            &fx,
            &format!("t{i}"),
            "alice@x",
            T0 + (i + 1) * DAY,
            20 * HOUR,
        );
    }
    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.ours.samples, 4);
    assert_eq!(alice.theirs.samples, 0);
    assert!(!alice.slower_than_counterpart);
}

#[tokio::test]
async fn the_bottleneck_ratio_is_honoured_rather_than_hardcoded() {
    let fx = Fx::open();
    // Three times slower than Alice.
    symmetric_exchange(&fx, 4, 3 * HOUR, HOUR);

    let mut strict = fx.query();
    strict.bottleneck_ratio = 5.0;
    assert!(
        !group(&fx.run(strict).await, "alice@x").slower_than_counterpart,
        "3x is not 5x"
    );

    let mut lax = fx.query();
    lax.bottleneck_ratio = 1.5;
    assert!(group(&fx.run(lax).await, "alice@x").slower_than_counterpart);
}

#[tokio::test]
async fn min_samples_is_honoured_rather_than_hardcoded() {
    let fx = Fx::open();
    // Two of ours, one of theirs — under the default bar of three.
    symmetric_exchange(&fx, 1, 10 * HOUR, HOUR);
    assert!(!group(&fx.run(fx.query()).await, "alice@x").slower_than_counterpart);

    let mut lenient = fx.query();
    lenient.min_samples = 1;
    assert!(
        group(&fx.run(lenient).await, "alice@x").slower_than_counterpart,
        "one observation each side is enough once the caller says so"
    );
}

#[tokio::test]
async fn a_median_of_seconds_is_never_a_bottleneck_however_instant_they_are() {
    let fx = Fx::open();
    // They answer in the same second; we take two. The ratio is unbounded and
    // the verdict must still be "no".
    symmetric_exchange(&fx, 4, 2, 0);

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.ours.p50_seconds, 2);
    assert_eq!(alice.theirs.p50_seconds, 0);
    assert!(
        !alice.slower_than_counterpart,
        "two seconds is not blocking a conversation, whatever the ratio says"
    );
    assert!(!alice.bottleneck);
}

#[tokio::test]
async fn abandoned_mail_from_a_real_correspondent_stalls_the_group() {
    let fx = Fx::open();
    // One answered exchange establishes the correspondence...
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    // ...then three separate threads she opens and we never touch.
    for i in 0..3 {
        fx.add(
            Msg::new(&format!("drop{i}@x"), "alice@x", T0 + (2 + i) * DAY)
                .subject(&format!("Q{i}")),
        );
    }

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.inbound, 4);
    assert_eq!(alice.awaiting_reply, 3);
    assert_eq!(alice.overdue, 3, "all three are weeks past our own p90");
    assert!(alice.stalled);
    assert!(alice.bottleneck);
    assert!(!alice.slower_than_counterpart, "the ratio arm did not fire");
}

#[tokio::test]
async fn a_later_message_of_ours_in_the_thread_answers_the_whole_thread() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    // Alice writes three more times in the *same* thread before we answer
    // once at the end. A message-exact rule would call three of them
    // abandoned; the thread was answered.
    for i in 0..3 {
        fx.add(
            Msg::new(&format!("more{i}@x"), "alice@x", T0 + (2 + i) * DAY)
                .subject("a")
                .reply_to("a-out@x")
                .refs("a-in@x a-out@x"),
        );
    }
    fx.add(
        Msg::new("final@x", ME, T0 + 6 * DAY)
            .subject("a")
            .reply_to("more2@x")
            .refs("a-in@x a-out@x more2@x"),
    );

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.inbound, 4);
    assert_eq!(alice.awaiting_reply, 0);
    assert_eq!(alice.overdue, 0);
    assert!(!alice.stalled);
}

#[tokio::test]
async fn a_conversation_that_merely_ended_on_their_message_is_not_a_bottleneck() {
    let fx = Fx::open();
    // Four threads, each closed by them within the hour after our reply —
    // "thanks!", "got it". Unanswered, but none of it is late: half of all
    // healthy correspondence looks exactly like this, and a flag that fires
    // here fires everywhere.
    let until = T0 + 30 * DAY;
    for i in 0..4 {
        let tag = format!("t{i}");
        let base = until - 8 * HOUR + i * HOUR;
        fx.add(Msg::new(&format!("{tag}-in@x"), "alice@x", base).subject(&tag));
        fx.add(
            Msg::new(&format!("{tag}-out@x"), ME, base + HOUR)
                .subject(&tag)
                .reply_to(&format!("{tag}-in@x")),
        );
        fx.add(
            Msg::new(&format!("{tag}-thanks@x"), "alice@x", base + 2 * HOUR)
                .subject(&tag)
                .reply_to(&format!("{tag}-out@x")),
        );
    }

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(alice.awaiting_reply, 4, "they did speak last, four times");
    assert_eq!(
        alice.overdue, 0,
        "nothing has been waiting longer than a day"
    );
    assert!(!alice.stalled);
    assert!(!alice.bottleneck);
}

#[tokio::test]
async fn the_overdue_bar_is_our_own_p90_when_that_exceeds_the_floor() {
    let fx = Fx::open();
    // We habitually take ten days with Bob. Three of his messages have been
    // waiting five — unanswered, but well inside how long he waits anyway.
    for i in 0..3 {
        exchange(&fx, &format!("s{i}"), "bob@x", T0 + i * DAY, 10 * DAY);
    }
    for i in 0..3 {
        fx.add(
            Msg::new(&format!("open{i}@x"), "bob@x", T0 + 25 * DAY + i * HOUR)
                .subject(&format!("Q{i}")),
        );
    }

    let report = fx.run(fx.query()).await;
    let bob = group(&report, "bob@x");
    assert_eq!(bob.ours.p90_seconds, 10 * DAY);
    assert_eq!(bob.awaiting_reply, 3);
    assert_eq!(
        bob.overdue, 0,
        "five days is not late for a correspondence whose p90 is ten"
    );
    assert!(!bob.stalled);
    assert_eq!(overdue_after(bob.ours), 10 * DAY);
    assert_eq!(overdue_after(Stats::default()), OVERDUE_FLOOR_SECONDS);
}

#[tokio::test]
async fn a_sender_we_have_never_replied_to_is_not_a_group() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    // A newsletter: ten messages, never once answered, never a reply from it.
    for i in 0..10 {
        fx.add(
            Msg::new(&format!("n{i}@x"), "news@x", T0 + DAY + i * HOUR).subject(&format!("N{i}")),
        );
    }

    let report = fx.run(fx.query()).await;
    assert!(
        report.groups.iter().all(|g| g.key != "news@x"),
        "listing every newsletter as a relationship you are blocking would bury the real ones"
    );
    assert_eq!(report.total_groups, 1);
}

#[tokio::test]
async fn an_unsent_draft_is_not_a_reply() {
    let fx = Fx::open();
    let drafts = fx.add_mailbox("Drafts");
    fx.add(Msg::new("in@x", "alice@x", T0 + DAY));
    // Started, never sent. It carries In-Reply-To and our address exactly
    // like a sent reply does.
    fx.add(
        Msg::new("draft@x", ME, T0 + DAY + HOUR)
            .reply_to("in@x")
            .in_mailbox(drafts),
    );

    let report = fx.run(fx.query()).await;
    assert_eq!(
        report.ours.samples, 0,
        "a reply you have written but not sent is not a response time"
    );
    assert_eq!(report.pairs, 0);
}

#[tokio::test]
async fn a_draft_does_not_make_a_thread_look_answered() {
    let fx = Fx::open();
    let drafts = fx.add_mailbox("Drafts");
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    for i in 0..3 {
        let tag = format!("q{i}");
        fx.add(Msg::new(&format!("{tag}-in@x"), "alice@x", T0 + (2 + i) * DAY).subject(&tag));
        fx.add(
            Msg::new(&format!("{tag}-draft@x"), ME, T0 + (2 + i) * DAY + HOUR)
                .subject(&tag)
                .reply_to(&format!("{tag}-in@x"))
                .in_mailbox(drafts),
        );
    }

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(
        alice.overdue, 3,
        "starting to reply is not replying; these are still owed"
    );
    assert!(alice.stalled);
}

#[tokio::test]
async fn mail_you_deleted_or_junked_is_mail_you_handled() {
    let fx = Fx::open();
    let trash = fx.add_mailbox("Trash");
    let junk = fx.add_mailbox("Junk");
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    for i in 0..3 {
        fx.add(
            Msg::new(&format!("t{i}@x"), "alice@x", T0 + (2 + i) * DAY)
                .subject("T")
                .in_mailbox(trash),
        );
        fx.add(
            Msg::new(&format!("j{i}@x"), "alice@x", T0 + (2 + i) * DAY)
                .subject("J")
                .in_mailbox(junk),
        );
    }

    let report = fx.run(fx.query()).await;
    let alice = group(&report, "alice@x");
    assert_eq!(
        alice.inbound, 1,
        "deleting or junking a message is a way of handling it"
    );
    assert_eq!(alice.awaiting_reply, 0);
    assert_eq!(alice.overdue, 0);
    assert!(!alice.stalled);
}

// ---------------------------------------------------------------------------
// Rolling trend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_trend_has_a_point_per_bucket_ending_at_until() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    let query = fx.query();
    let (since, until, bucket) = (query.since, query.until, query.bucket_seconds);
    let report = fx.run(query).await;

    // 30 days at a 7-day bucket: ceil(30/7) = 5 points.
    assert_eq!(report.trend.len(), 5);
    assert_eq!(
        report.trend.last().map(|p| p.window_end),
        Some(until),
        "the newest point must end exactly at the window's end"
    );
    let ends: Vec<i64> = report.trend.iter().map(|p| p.window_end).collect();
    for pair in ends.windows(2) {
        assert_eq!(pair[1] - pair[0], bucket);
    }
    assert!(report.trend.iter().all(|p| p.window_start >= since));
}

#[tokio::test]
async fn early_trend_points_are_clamped_to_since_rather_than_reading_further_back() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    let report = fx.run(fx.query()).await;
    let first = report.trend.first().unwrap();
    assert_eq!(
        first.window_start, report.since,
        "a 28-day rolling window on day 2 must not claim to cover day -26"
    );
    assert!(first.window_end > first.window_start);
}

#[tokio::test]
async fn the_trend_shows_a_median_getting_worse() {
    let fx = Fx::open();
    // Fast in week one, slow in week four. A short rolling window so the two
    // do not overlap in the same point.
    for i in 0..3 {
        exchange(&fx, &format!("f{i}"), "alice@x", T0 + i * HOUR, HOUR);
    }
    for i in 0..3 {
        exchange(
            &fx,
            &format!("s{i}"),
            "alice@x",
            T0 + 22 * DAY + i * DAY,
            48 * HOUR,
        );
    }

    let mut query = fx.query();
    query.window_seconds = 7 * DAY;
    let report = fx.run(query).await;

    let first = report.trend.first().unwrap();
    let last = report.trend.last().unwrap();
    assert_eq!(first.stats.samples, 3);
    assert_eq!(first.stats.p50_seconds, HOUR);
    assert_eq!(last.stats.samples, 3);
    assert_eq!(last.stats.p50_seconds, 48 * HOUR);
}

#[tokio::test]
async fn a_bucket_with_no_replies_reports_zero_samples_not_a_gap() {
    let fx = Fx::open();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);
    let mut query = fx.query();
    query.window_seconds = 7 * DAY;
    let report = fx.run(query).await;

    assert_eq!(report.trend.len(), 5);
    assert_eq!(report.trend.first().unwrap().stats.samples, 1);
    assert!(
        report.trend.iter().skip(2).all(|p| p.stats.samples == 0),
        "an empty rolling window is a reported zero, not a missing point"
    );
}

#[tokio::test]
async fn the_trend_covers_only_our_own_replies() {
    let fx = Fx::open();
    // Only *their* replies exist. The trend is about our responsiveness, so
    // it must stay empty even though pairs were matched.
    for i in 0..3 {
        fx.add(Msg::new(&format!("o{i}@x"), ME, T0 + (i + 1) * DAY).subject(&format!("t{i}")));
        fx.add(
            Msg::new(&format!("i{i}@x"), "alice@x", T0 + (i + 1) * DAY + HOUR)
                .subject(&format!("t{i}"))
                .reply_to(&format!("o{i}@x")),
        );
    }
    let report = fx.run(fx.query()).await;
    assert_eq!(report.theirs.samples, 3);
    assert!(report.trend.iter().all(|p| p.stats.samples == 0));
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_account_username_is_an_identity_even_with_an_empty_sent_folder() {
    let fx = Fx::open();
    // Everything lands in INBOX; nothing was ever filed in Sent.
    fx.add(Msg::new("in@x", "alice@x", T0 + DAY).in_mailbox(fx.inbox));
    fx.add(
        Msg::new("out@x", ME, T0 + DAY + HOUR)
            .reply_to("in@x")
            .in_mailbox(fx.inbox),
    );

    let report = fx.run(fx.query()).await;
    assert_eq!(report.self_addresses, vec![ME.to_owned()]);
    assert_eq!(report.ours.samples, 1);
}

#[tokio::test]
async fn an_alias_is_learned_from_the_sent_folder() {
    let fx = Fx::open();
    fx.add(Msg::new("in@x", "alice@x", T0 + DAY));
    // Sent from the alias, filed in Sent — which is the only place rmail can
    // learn that the alias is us.
    fx.add(Msg::new("out@x", ALIAS, T0 + DAY + 2 * HOUR).reply_to("in@x"));

    let report = fx.run(fx.query()).await;
    assert!(report.self_addresses.contains(&ALIAS.to_owned()));
    assert_eq!(report.ours.samples, 1);
    assert_eq!(report.ours.p50_seconds, 2 * HOUR);
}

#[tokio::test]
async fn addresses_match_case_insensitively() {
    let fx = Fx::open();
    fx.add(Msg::new("in@x", "Alice@X", T0 + DAY));
    fx.add(Msg::new("out@x", "ME@Example.COM", T0 + DAY + HOUR).reply_to("in@x"));

    let report = fx.run(fx.query()).await;
    assert_eq!(report.ours.samples, 1);
    assert_eq!(group(&report, "alice@x").key, "alice@x");
}

#[tokio::test]
async fn a_non_ascii_address_normalizes_the_same_way_the_query_does() {
    let fx = Fx::open_with_username(Some("MÜLLER@example.com"));
    // This module's `normalize_address` must reproduce SQLite's
    // `lower(trim(...))`, which is ASCII-only. A Rust `to_lowercase()` would
    // fold the `Ü` the database leaves alone; the identity would then match
    // nothing inside the answered-ness query, and every thread would come
    // back unanswered.
    fx.add(Msg::new("in1@x", "alice@x", T0 + DAY).subject("A"));
    fx.add(
        Msg::new("out1@x", "MÜLLER@example.com", T0 + DAY + HOUR)
            .subject("A")
            .reply_to("in1@x"),
    );
    fx.add(Msg::new("in2@x", "alice@x", T0 + 2 * DAY).subject("B"));

    let report = fx.run(fx.query()).await;
    assert_eq!(report.self_addresses, vec!["mÜller@example.com".to_owned()]);
    assert_eq!(report.ours.samples, 1);
    let alice = group(&report, "alice@x");
    assert_eq!(
        alice.awaiting_reply, 1,
        "only the unanswered thread is owed — the answered one must be seen \
         as answered, which needs the identity to match inside SQLite"
    );
}

#[tokio::test]
async fn with_no_known_identity_the_report_is_empty_and_says_why() {
    let fx = Fx::open_with_username(None);
    // A complete exchange exists, but nothing tells rmail which side is the
    // user: no username, and the reply is not filed in Sent.
    fx.add(Msg::new("in@x", "alice@x", T0 + DAY).in_mailbox(fx.inbox));
    fx.add(
        Msg::new("out@x", ME, T0 + DAY + HOUR)
            .reply_to("in@x")
            .in_mailbox(fx.inbox),
    );

    let report = fx.run(fx.query()).await;
    assert!(
        report.self_addresses.is_empty(),
        "the empty identity set is the explanation, and must be reported"
    );
    assert_eq!(report.pairs, 0);
    assert_eq!(report.ours.samples, 0);
}

#[tokio::test]
async fn a_username_that_is_not_an_address_is_not_an_identity() {
    let fx = Fx::open_with_username(Some("alice"));
    fx.add(Msg::new("in@x", "bob@x", T0 + DAY).in_mailbox(fx.inbox));
    fx.add(
        Msg::new("out@x", ME, T0 + DAY + HOUR)
            .reply_to("in@x")
            .in_mailbox(fx.inbox),
    );

    let report = fx.run(fx.query()).await;
    assert!(
        report.self_addresses.is_empty(),
        "a login of `alice` says nothing about what she sends as: {:?}",
        report.self_addresses
    );
}

#[tokio::test]
async fn another_accounts_mail_is_not_in_a_scoped_report() {
    let fx = Fx::open();
    let (other_account, other_inbox) = fx
        .db
        .with_write(|c| {
            let account_id = repo::insert_account(
                c,
                &repo::NewAccount {
                    name: "Work".to_owned(),
                    username: Some("me@work.example".to_owned()),
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
        .unwrap();
    fx.db
        .with_write(|c| {
            for (uid, message_id, from, at, parent) in [
                (900, "w-in@x", "carol@x", T0 + DAY, None),
                (
                    901,
                    "w-out@x",
                    "me@work.example",
                    T0 + DAY + HOUR,
                    Some("w-in@x"),
                ),
            ] {
                let id = repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id: other_account,
                        mailbox_id: other_inbox,
                        uid,
                        uidvalidity: 1,
                        message_id: Some(message_id.to_owned()),
                        in_reply_to: parent.map(str::to_owned),
                        subject: Some("Work".to_owned()),
                        from_addr: Some(from.to_owned()),
                        date: Some(at),
                        ..Default::default()
                    },
                )?;
                assign_thread(c, id)?;
            }
            Ok(())
        })
        .unwrap();
    exchange(&fx, "a", "alice@x", T0 + DAY, HOUR);

    let scoped = fx.run(fx.query()).await;
    assert_eq!(scoped.ours.samples, 1);
    assert_eq!(scoped.self_addresses, vec![ME.to_owned()]);

    let mut all = fx.query();
    all.account_id = None;
    let across = fx.run(all).await;
    assert_eq!(across.ours.samples, 2, "unscoped covers every account");
    assert_eq!(
        across.self_addresses,
        vec!["me@example.com".to_owned(), "me@work.example".to_owned()]
    );
}
