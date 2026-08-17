//! Contact insights: volume and direction, response symmetry, cadence,
//! topics, the decay report — and the degenerate shapes analytics is only easy
//! to get right on tidy data: an empty range, a contact with exactly one
//! message, a contact you have never replied to, and every place a naive
//! implementation divides by zero.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::analytics::response_time::{self as rt, GroupBy, ResponseTimeQuery};
use crate::repo;
use crate::thread::assign_thread;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 2023-11-14T22:13:20Z — fixed, so every assertion is about a difference.
const T0: i64 = 1_700_000_000;
const HOUR: i64 = 3_600;
const ME: &str = "me@example.com";
const ADA: &str = "ada@example.com";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Msg<'a> {
    message_id: Option<&'a str>,
    in_reply_to: Option<&'a str>,
    subject: Option<&'a str>,
    from: Option<&'a str>,
    from_name: Option<&'a str>,
    to: Option<&'a str>,
    cc: Option<&'a str>,
    at: Option<i64>,
    mailbox: Option<i64>,
}

impl<'a> Msg<'a> {
    /// From the contact to you.
    fn inbound(id: &'a str, at: i64) -> Self {
        Self {
            message_id: Some(id),
            from: Some(ADA),
            to: Some(ME),
            at: Some(at),
            subject: Some("Lease renewal"),
            ..Default::default()
        }
    }
    /// From you to the contact.
    fn outbound(id: &'a str, at: i64) -> Self {
        Self {
            message_id: Some(id),
            from: Some(ME),
            to: Some(ADA),
            at: Some(at),
            subject: Some("Re: Lease renewal"),
            ..Default::default()
        }
    }
    fn reply_to(mut self, parent: &'a str) -> Self {
        self.in_reply_to = Some(parent);
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
    fn to(mut self, to: &'a str) -> Self {
        self.to = Some(to);
        self
    }
    fn cc(mut self, cc: &'a str) -> Self {
        self.cc = Some(cc);
        self
    }
    fn from(mut self, from: &'a str) -> Self {
        self.from = Some(from);
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
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-contacts-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        let (account_id, inbox, sent) = db
            .with_write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: format!("Personal-{n}"),
                        username: Some(ME.to_owned()),
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

    fn add(&self, msg: Msg<'_>) -> i64 {
        let outbound = msg.from.is_some_and(|from| from.eq_ignore_ascii_case(ME));
        let mailbox_id = msg
            .mailbox
            .unwrap_or(if outbound { self.sent } else { self.inbox });
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            message_id: msg.message_id.map(str::to_owned),
            in_reply_to: msg.in_reply_to.map(str::to_owned),
            subject: msg.subject.map(str::to_owned),
            from_addr: msg.from.map(str::to_owned),
            from_name: msg.from_name.map(str::to_owned),
            to_addrs: msg.to.map(str::to_owned),
            cc_addrs: msg.cc.map(str::to_owned),
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

    /// A query over `[T0, T0 + 30 days)`.
    fn query(&self) -> ContactInsightQuery {
        ContactInsightQuery {
            account_id: Some(self.account_id),
            address: ADA.to_owned(),
            since: T0,
            until: T0 + 30 * DAY,
            topic_limit: DEFAULT_TOPIC_LIMIT,
            metrics_only: true,
        }
    }

    async fn run(&self, query: ContactInsightQuery) -> ContactInsight {
        metrics(&self.db, &CancellationToken::new(), query)
            .await
            .unwrap()
    }

    async fn run_err(&self, query: ContactInsightQuery) -> Error {
        metrics(&self.db, &CancellationToken::new(), query)
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

// ---------------------------------------------------------------------------
// Volume and direction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn volume_counts_both_directions_and_the_threads_they_share() {
    let fx = Fx::open();
    fx.add(Msg::inbound("a1", T0 + HOUR).name("Ada Lovelace"));
    fx.add(Msg::outbound("m1", T0 + 3 * HOUR).reply_to("a1"));
    fx.add(
        Msg::inbound("a2", T0 + 5 * HOUR)
            .subject("Invoice")
            .reply_to("m1"),
    );

    let insight = fx.run(fx.query()).await;
    assert_eq!(insight.address, ADA);
    assert_eq!(insight.name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(insight.volume.inbound, 2);
    assert_eq!(insight.volume.outbound, 1);
    assert_eq!(insight.volume.threads, 1);
    assert_eq!(insight.volume.first_seen, Some(T0 + HOUR));
    assert_eq!(insight.volume.last_inbound, Some(T0 + 5 * HOUR));
    assert_eq!(insight.volume.last_outbound, Some(T0 + 3 * HOUR));
    let ratio = insight.volume.direction_ratio().unwrap();
    assert!((ratio - 1.0 / 3.0).abs() < 1e-9, "{ratio}");
    assert_eq!(insight.accounts, vec![fx.account_id]);
}

/// A `cc:` counts. The alternative — only `to:` — would report a
/// correspondence you are half of as one you are not in at all.
#[tokio::test]
async fn mail_cc_to_the_contact_counts_as_outbound() {
    let fx = Fx::open();
    fx.add(
        Msg::outbound("m1", T0 + HOUR)
            .to("carol@example.com")
            .cc(ADA),
    );
    let insight = fx.run(fx.query()).await;
    assert_eq!(insight.volume.outbound, 1);
}

/// The `instr` narrowing in SQL is allowed to over-match; `addressed_to` is
/// what decides. If that were a substring test, this would report one.
#[tokio::test]
async fn a_lookalike_recipient_is_not_the_contact() {
    let fx = Fx::open();
    fx.add(Msg::outbound("m1", T0 + HOUR).to("malice@example.com"));
    fx.add(Msg::outbound("m2", T0 + 2 * HOUR).to("ada@example.com.evil.test"));
    let insight = fx.run(fx.query()).await;
    assert_eq!(
        insight.volume.outbound, 0,
        "a substring match let a different address through"
    );
}

#[test]
fn addressed_to_is_exact_on_the_delimiters() {
    assert!(addressed_to(Some("ada@example.com"), ADA));
    assert!(addressed_to(Some("bob@example.com, ada@example.com"), ADA));
    assert!(addressed_to(Some("Ada <ada@example.com>"), ADA));
    assert!(addressed_to(Some(" ADA@EXAMPLE.COM "), ADA));
    assert!(!addressed_to(Some("malice@example.com"), ADA));
    assert!(!addressed_to(Some("ada@example.com.evil.test"), ADA));
    assert!(!addressed_to(None, ADA));
    assert!(!addressed_to(Some(""), ADA));
}

/// Trash and Drafts are excluded, exactly as they are in the response-time
/// report — a draft reply has not answered anything and a junked message has
/// been handled.
#[tokio::test]
async fn disposed_and_draft_folders_are_excluded() {
    let fx = Fx::open();
    let trash = fx.add_mailbox("Trash");
    let drafts = fx.add_mailbox("Drafts");
    fx.add(Msg::inbound("a1", T0 + HOUR).in_mailbox(trash));
    fx.add(Msg::outbound("m1", T0 + 2 * HOUR).in_mailbox(drafts));
    fx.add(Msg::inbound("a2", T0 + 3 * HOUR));

    let insight = fx.run(fx.query()).await;
    assert_eq!(insight.volume.inbound, 1);
    assert_eq!(insight.volume.outbound, 0);
}

// ---------------------------------------------------------------------------
// Symmetry, and the general path it specializes
// ---------------------------------------------------------------------------

/// The specialization must agree with `response_times` on the same window.
/// This is the test that stops the two drifting: if either changes its pairing
/// rule, its direction test or its percentile method, they stop matching.
#[tokio::test]
async fn contact_insight_matches_the_response_time_report() {
    let fx = Fx::open();
    fx.add(Msg::inbound("a1", T0 + HOUR));
    fx.add(Msg::outbound("m1", T0 + 4 * HOUR).reply_to("a1"));
    fx.add(Msg::inbound("a2", T0 + 6 * HOUR).reply_to("m1"));
    fx.add(Msg::outbound("m2", T0 + 30 * HOUR).reply_to("a2"));
    fx.add(Msg::inbound("a3", T0 + 31 * HOUR).reply_to("m2"));

    let insight = fx.run(fx.query()).await;
    let report = crate::analytics::response_times(
        &fx.db,
        &CancellationToken::new(),
        ResponseTimeQuery {
            account_id: Some(fx.account_id),
            group_by: GroupBy::Contact,
            since: T0,
            until: T0 + 30 * DAY,
            bucket_seconds: 7 * DAY,
            window_seconds: 28 * DAY,
            limit: rt::DEFAULT_LIMIT,
            min_samples: rt::DEFAULT_MIN_SAMPLES,
            bottleneck_ratio: rt::DEFAULT_BOTTLENECK_RATIO,
        },
    )
    .await
    .unwrap();
    let group = report
        .groups
        .iter()
        .find(|group| group.key == ADA)
        .unwrap_or_else(|| panic!("no group for {ADA} in {:?}", report.groups));

    assert_eq!(insight.ours, group.ours, "ours");
    assert_eq!(insight.theirs, group.theirs, "theirs");
    assert_eq!(insight.awaiting_reply, group.awaiting_reply, "awaiting");
    assert_eq!(insight.overdue, group.overdue, "overdue");
    assert_eq!(insight.volume.inbound, group.inbound, "inbound");
}

#[tokio::test]
async fn symmetry_is_theirs_over_ours() {
    let fx = Fx::open();
    // You take 4h; they take 2h.
    fx.add(Msg::inbound("a1", T0 + HOUR));
    fx.add(Msg::outbound("m1", T0 + 5 * HOUR).reply_to("a1"));
    fx.add(Msg::inbound("a2", T0 + 7 * HOUR).reply_to("m1"));

    let insight = fx.run(fx.query()).await;
    assert_eq!(insight.ours.p50_seconds, 4 * HOUR);
    assert_eq!(insight.theirs.p50_seconds, 2 * HOUR);
    let symmetry = insight.symmetry.unwrap();
    assert!((symmetry - 0.5).abs() < 1e-9, "{symmetry}");
}

/// A correspondent you have never replied to has no symmetry. Reporting 0.0
/// would read as "they are infinitely faster than you", which is a claim about
/// data that does not exist.
#[tokio::test]
async fn symmetry_is_absent_when_one_side_never_replied() {
    let fx = Fx::open();
    fx.add(Msg::inbound("a1", T0 + HOUR));
    fx.add(Msg::inbound("a2", T0 + 2 * HOUR));
    let insight = fx.run(fx.query()).await;
    assert_eq!(insight.ours.samples, 0);
    assert_eq!(insight.theirs.samples, 0);
    assert_eq!(insight.symmetry, None);
}

/// An auto-responder answers in the same second. The denominator is floored at
/// one, so this is a large ratio rather than a division by zero.
#[tokio::test]
async fn a_zero_median_does_not_divide_by_zero() {
    let fx = Fx::open();
    // You reply instantly; they take an hour.
    fx.add(Msg::inbound("a1", T0 + HOUR));
    fx.add(Msg::outbound("m1", T0 + HOUR).reply_to("a1"));
    fx.add(Msg::inbound("a2", T0 + 2 * HOUR).reply_to("m1"));

    let insight = fx.run(fx.query()).await;
    assert_eq!(insight.ours.p50_seconds, 0);
    let symmetry = insight.symmetry.unwrap();
    assert!(symmetry.is_finite(), "{symmetry}");
    assert!((symmetry - HOUR as f64).abs() < 1e-9, "{symmetry}");
}

// ---------------------------------------------------------------------------
// Cadence and decay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_message_has_no_gap_and_no_trend() {
    let fx = Fx::open();
    fx.add(Msg::inbound("a1", T0 + HOUR));

    let insight = fx.run(fx.query()).await;
    assert_eq!(insight.volume.inbound, 1);
    assert_eq!(
        insight.cadence.median_gap_seconds, None,
        "one message cannot have a gap"
    );
    assert_eq!(insight.cadence.longest_gap_seconds, None);
    assert_eq!(insight.decay.prior_messages, 1);
    assert_eq!(insight.decay.recent_messages, 0);
    assert_eq!(
        insight.decay.change_ratio,
        Some(0.0),
        "one early message and nothing since is a real decline, not an undefined one"
    );
    // The floor applies, since there is no cadence to calibrate against.
    assert_eq!(insight.decay.dormant_after_seconds, DORMANT_FLOOR_SECONDS);
}

/// The window is empty. Every derived number has to be a *statement about
/// nothing*, not a zero that reads as a measurement.
#[tokio::test]
async fn an_empty_range_reports_nothing_rather_than_zeroes_that_look_measured() {
    let fx = Fx::open();
    fx.add(Msg::inbound("a1", T0 - 10 * DAY));

    let insight = fx.run(fx.query()).await;
    assert_eq!(insight.volume.inbound, 0);
    assert_eq!(insight.volume.outbound, 0);
    assert_eq!(insight.volume.direction_ratio(), None);
    assert_eq!(insight.volume.first_seen, None);
    assert_eq!(insight.cadence.median_gap_seconds, None);
    assert!((insight.cadence.messages_per_week - 0.0).abs() < 1e-9);
    assert_eq!(insight.decay.silence_seconds, None);
    assert_eq!(insight.decay.change_ratio, None);
    assert!(insight.decay.dormant, "no exchange at all is dormant");
    assert!(
        !insight.decay.declining,
        "an empty window is not a declining relationship, it is no relationship"
    );
    assert_eq!(insight.symmetry, None);
    assert!(insight.topics.is_empty());
    assert!(insight.accounts.is_empty());
}

#[tokio::test]
async fn a_steady_correspondence_is_neither_dormant_nor_declining() {
    let fx = Fx::open();
    let ids = ["a1", "a2", "a3", "a4", "a5", "a6"];
    for (index, id) in ids.iter().enumerate() {
        let at = T0 + (index as i64 + 1) * 4 * DAY;
        fx.add(Msg::inbound(id, at));
    }
    let insight = fx.run(fx.query()).await;
    assert_eq!(insight.cadence.median_gap_seconds, Some(4 * DAY));
    assert_eq!(insight.decay.prior_messages, 3);
    assert_eq!(insight.decay.recent_messages, 3);
    assert_eq!(insight.decay.change_ratio, Some(1.0));
    assert!(!insight.decay.declining);
    assert!(!insight.decay.dormant, "the last message is 6 days old");
}

#[tokio::test]
async fn a_correspondence_that_stopped_is_dormant_and_declining() {
    let fx = Fx::open();
    // Five messages in the first three days, nothing after.
    for (index, id) in ["a1", "a2", "a3", "a4", "a5"].iter().enumerate() {
        fx.add(Msg::inbound(id, T0 + index as i64 * 12 * HOUR));
    }
    let mut query = fx.query();
    query.until = T0 + 120 * DAY;
    let insight = fx.run(query).await;
    assert!(insight.decay.dormant, "{:?}", insight.decay);
    assert!(insight.decay.declining, "{:?}", insight.decay);
    assert_eq!(insight.decay.recent_messages, 0);
    assert_eq!(insight.decay.change_ratio, Some(0.0));
}

/// A daily correspondent silent for a week is not dormant; the threshold is
/// this pair's own cadence, floored at 30 days.
#[tokio::test]
async fn the_dormancy_threshold_is_floored_for_a_fast_correspondence() {
    let fx = Fx::open();
    for (index, id) in ["a1", "a2", "a3"].iter().enumerate() {
        fx.add(Msg::inbound(id, T0 + index as i64 * HOUR));
    }
    let mut query = fx.query();
    query.until = T0 + 7 * DAY;
    let insight = fx.run(query).await;
    assert_eq!(insight.cadence.median_gap_seconds, Some(HOUR));
    assert_eq!(
        insight.decay.dormant_after_seconds, DORMANT_FLOOR_SECONDS,
        "3 * 1h is under the floor, so the floor applies"
    );
    assert!(
        !insight.decay.dormant,
        "a week of silence is not dormancy this soon"
    );
}

// ---------------------------------------------------------------------------
// Topics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn topics_are_recurring_subject_terms_counted_per_message() {
    let fx = Fx::open();
    fx.add(Msg::inbound("a1", T0 + HOUR).subject("Re: Lease renewal lease lease"));
    fx.add(Msg::inbound("a2", T0 + 2 * HOUR).subject("Lease renewal timeline"));
    fx.add(Msg::inbound("a3", T0 + 3 * HOUR).subject("Fwd: the 2024 invoice"));

    let insight = fx.run(fx.query()).await;
    let terms: Vec<&str> = insight.topics.iter().map(|t| t.term.as_str()).collect();
    assert!(terms.contains(&"lease"), "{terms:?}");
    assert!(terms.contains(&"renewal"), "{terms:?}");
    // `re`/`fwd`/`the` are mail furniture and stopwords; `2024` is a number.
    for noise in ["re", "fwd", "the", "2024"] {
        assert!(!terms.contains(&noise), "{noise} survived: {terms:?}");
    }
    let lease = insight.topics.iter().find(|t| t.term == "lease").unwrap();
    assert_eq!(
        lease.messages, 2,
        "a subject repeating a word must contribute once"
    );
    // A term seen in only one message is not recurring.
    assert!(!terms.contains(&"timeline"), "{terms:?}");
}

#[tokio::test]
async fn the_topic_limit_is_honoured_and_clamped() {
    let fx = Fx::open();
    for (index, id) in ["a1", "a2", "a3"].iter().enumerate() {
        fx.add(
            Msg::inbound(id, T0 + index as i64 * HOUR)
                .subject("alpha bravo charlie delta echo foxtrot"),
        );
    }
    let mut query = fx.query();
    query.topic_limit = 2;
    assert_eq!(fx.run(query).await.topics.len(), 2);

    let mut query = fx.query();
    query.topic_limit = 10_000;
    let insight = fx.run(query).await;
    assert!(insight.topics.len() <= MAX_TOPIC_LIMIT);
}

// ---------------------------------------------------------------------------
// Errors and bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_blank_address_is_rejected() {
    let fx = Fx::open();
    let mut query = fx.query();
    query.address = "   ".to_owned();
    let error = fx.run_err(query).await;
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn an_inverted_window_is_rejected() {
    let fx = Fx::open();
    let mut query = fx.query();
    query.since = query.until;
    let error = fx.run_err(query).await;
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(error.to_string().contains("strictly before"), "{error}");
}

#[tokio::test]
async fn the_address_is_normalized_before_anything_else() {
    let fx = Fx::open();
    fx.add(Msg::inbound("a1", T0 + HOUR));
    let mut query = fx.query();
    query.address = "  ADA@Example.COM ".to_owned();
    let insight = fx.run(query).await;
    assert_eq!(insight.address, ADA);
    assert_eq!(insight.volume.inbound, 1);
}

/// A cancelled scan is an error, never an empty report — the same rule
/// `response_time::scan` enforces, for the same reason.
#[tokio::test]
async fn a_cancelled_scan_errors_rather_than_reporting_an_empty_correspondence() {
    let fx = Fx::open();
    fx.add(Msg::inbound("a1", T0 + HOUR));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = metrics(&fx.db, &cancel, fx.query()).await.unwrap_err();
    assert_eq!(error.reason(), ErrorReason::Cancelled);
}

/// Mail from a third party that merely shares a thread with the contact is not
/// this contact's latency.
#[tokio::test]
async fn a_third_partys_reply_is_not_attributed_to_the_contact() {
    let fx = Fx::open();
    fx.add(
        Msg::inbound("c1", T0 + HOUR)
            .from("carol@example.com")
            .to(ME),
    );
    // Your reply goes to Carol and cc's Ada, so the outbound scan admits it.
    fx.add(
        Msg::outbound("m1", T0 + 10 * HOUR)
            .reply_to("c1")
            .to("carol@example.com")
            .cc(ADA),
    );
    let insight = fx.run(fx.query()).await;
    assert_eq!(
        insight.volume.outbound, 1,
        "the cc is still your mail to Ada"
    );
    assert_eq!(
        insight.ours.samples, 0,
        "a reply to Carol is not a reply to Ada"
    );
}

// ---------------------------------------------------------------------------
// The prompt payload
// ---------------------------------------------------------------------------

/// The facts block must never carry a field whose value means nothing. A
/// `response_symmetry: 0` line against a contact who never replied is a number
/// the model would narrate.
#[test]
fn the_facts_block_omits_undefined_numbers() {
    let insight = ContactInsight {
        address: ADA.to_owned(),
        name: None,
        since: T0,
        until: T0 + 30 * DAY,
        volume: Volume::default(),
        ours: Stats::default(),
        theirs: Stats::default(),
        symmetry: None,
        awaiting_reply: 0,
        overdue: 0,
        accounts: Vec::new(),
        cadence: Cadence::default(),
        decay: Decay::default(),
        topics: Vec::new(),
        briefing: Briefing::default(),
    };
    let facts = facts(&insight);
    for absent in [
        "response_symmetry",
        "your_median_reply_seconds",
        "their_median_reply_seconds",
        "median_gap_seconds",
        "seconds_since_last_message",
        "share_written_by_you",
        "display_name",
        "subject_topics",
    ] {
        assert!(
            !facts.contains(absent),
            "{absent} was rendered with no value behind it:\n{facts}"
        );
    }
    assert!(facts.contains("messages_from_them: 0"));
    assert!(facts.contains("dormant: "));
}

/// Every value in the block is a number, an enum, or a term already reduced to
/// alphanumerics — so nothing in it can forge a line. The display name is the
/// one field that comes straight out of mail, and the block is fenced whole.
#[test]
fn the_facts_block_is_a_flat_key_value_list() {
    let insight = ContactInsight {
        address: ADA.to_owned(),
        name: Some("Ada".to_owned()),
        since: T0,
        until: T0 + 30 * DAY,
        volume: Volume {
            inbound: 3,
            outbound: 2,
            threads: 1,
            first_seen: Some(T0),
            last_inbound: Some(T0 + HOUR),
            last_outbound: Some(T0 + 2 * HOUR),
        },
        ours: Stats::from_sorted(&[HOUR]),
        theirs: Stats::from_sorted(&[2 * HOUR]),
        symmetry: Some(2.0),
        awaiting_reply: 1,
        overdue: 0,
        accounts: vec![1],
        cadence: Cadence {
            median_gap_seconds: Some(HOUR),
            longest_gap_seconds: Some(2 * HOUR),
            messages_per_week: 1.5,
        },
        decay: Decay::default(),
        topics: vec![Topic {
            term: "lease".to_owned(),
            messages: 2,
        }],
        briefing: Briefing::default(),
    };
    let facts = facts(&insight);
    for line in facts.lines().filter(|l| !l.is_empty()) {
        assert!(line.contains(": "), "not a key/value line: {line:?}");
    }
    assert!(facts.contains("subject_topics: lease (2)"), "{facts}");
    assert!(facts.contains("response_symmetry: 2.00"), "{facts}");
}
