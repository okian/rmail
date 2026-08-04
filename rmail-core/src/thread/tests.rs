//! Threading tests: reference chains, subject fallback, out-of-order arrival,
//! merging, and the derived thread aggregates.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::repo;
use crate::storage::Database;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// Base timestamp for fixtures (2023-11-14T22:13:20Z).
const T0: i64 = 1_700_000_000;

/// A message to insert, described only by the fields threading cares about.
#[derive(Default)]
struct Msg<'a> {
    message_id: Option<&'a str>,
    in_reply_to: Option<&'a str>,
    references: Option<&'a str>,
    subject: Option<&'a str>,
    from: Option<&'a str>,
    to: Option<&'a str>,
    cc: Option<&'a str>,
    date: Option<i64>,
}

impl<'a> Msg<'a> {
    fn id(mut self, id: &'a str) -> Self {
        self.message_id = Some(id);
        self
    }
    fn reply_to(mut self, id: &'a str) -> Self {
        self.in_reply_to = Some(id);
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
    fn from(mut self, from: &'a str) -> Self {
        self.from = Some(from);
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
    fn at(mut self, date: i64) -> Self {
        self.date = Some(date);
        self
    }
}

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
    mailbox_id: i64,
    next_uid: Cell<i64>,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-thread-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        let (account_id, mailbox_id) = db
            .with_write(|c| {
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
            .unwrap();
        Self {
            db,
            path,
            account_id,
            mailbox_id,
            next_uid: Cell::new(1),
        }
    }

    /// Add another account, returning `(account_id, mailbox_id)`.
    fn add_account(&self, name: &str) -> (i64, i64) {
        self.db
            .with_write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: name.to_owned(),
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

    /// Insert a message into the default account/mailbox and thread it.
    fn add(&self, msg: Msg<'_>) -> (i64, ThreadAssignment) {
        self.add_in(self.account_id, self.mailbox_id, msg)
    }

    /// Insert a message into a specific account/mailbox and thread it.
    fn add_in(&self, account_id: i64, mailbox_id: i64, msg: Msg<'_>) -> (i64, ThreadAssignment) {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            message_id: msg.message_id.map(str::to_owned),
            thread_id: None,
            in_reply_to: msg.in_reply_to.map(str::to_owned),
            references_hdr: msg.references.map(str::to_owned),
            subject: msg.subject.map(str::to_owned),
            from_addr: msg.from.map(str::to_owned),
            to_addrs: msg.to.map(str::to_owned),
            cc_addrs: msg.cc.map(str::to_owned),
            date: msg.date,
            ..Default::default()
        };
        self.db
            .with_write(|c| {
                let message_id = repo::insert_message(c, &new)?;
                let assignment = assign_thread(c, message_id)?;
                Ok((message_id, assignment))
            })
            .map(|(id, assignment)| (id, assignment.expect("message exists")))
            .unwrap()
    }

    fn thread(&self, id: i64) -> repo::Thread {
        self.db
            .with_read(|c| repo::get_thread(c, id))
            .unwrap()
            .expect("thread exists")
    }

    fn thread_of(&self, message_id: i64) -> Option<i64> {
        self.db
            .with_read(|c| repo::get_message(c, message_id))
            .unwrap()
            .expect("message exists")
            .thread_id
    }

    fn thread_count(&self) -> i64 {
        self.db
            .with_read(|c| c.query_row("SELECT count(*) FROM threads", [], |r| r.get(0)))
            .unwrap()
    }

    fn ref_count(&self) -> i64 {
        self.db
            .with_read(|c| c.query_row("SELECT count(*) FROM thread_refs", [], |r| r.get(0)))
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

// ---------------------------------------------------------------------------
// Reference chains
// ---------------------------------------------------------------------------

#[test]
fn reference_chain_forms_one_thread() {
    let fx = Fixture::open();
    let (a, first) = fx.add(
        Msg::default()
            .id("a@x")
            .subject("Project")
            .from("Alice@x")
            .to("bob@x")
            .at(T0),
    );
    let (b, second) = fx.add(
        Msg::default()
            .id("b@x")
            .reply_to("a@x")
            .refs("a@x")
            .subject("Re: Project")
            .from("bob@x")
            .to("alice@x")
            .at(T0 + 60),
    );
    let (c, third) = fx.add(
        Msg::default()
            .id("c@x")
            .reply_to("b@x")
            .refs("a@x b@x")
            .subject("Re: Re: Project")
            .from("carol@x")
            .to("alice@x, bob@x")
            .at(T0 + 120),
    );

    assert_eq!(first.link, ThreadLink::New);
    assert_eq!(second.link, ThreadLink::References);
    assert_eq!(third.link, ThreadLink::References);
    assert_eq!(second.thread_id, first.thread_id);
    assert_eq!(third.thread_id, first.thread_id);
    assert_eq!(fx.thread_count(), 1);

    let thread = fx.thread(first.thread_id);
    assert_eq!(thread.message_count, 3);
    assert_eq!(
        thread.root_message_id,
        Some(a),
        "oldest message is the root"
    );
    assert_eq!(thread.last_message_at, Some(T0 + 120));
    assert_eq!(thread.subject_norm.as_deref(), Some("project"));
    assert_eq!(
        thread.participant_list(),
        vec!["alice@x", "bob@x", "carol@x"],
        "participants are deduped, lowercased and sorted"
    );

    let members = fx
        .db
        .with_read(|c| repo::list_thread_message_ids(c, first.thread_id))
        .unwrap();
    assert_eq!(members, vec![a, b, c], "conversation reads oldest first");
}

#[test]
fn phantom_parent_links_siblings_whose_parent_was_never_fetched() {
    let fx = Fixture::open();
    // Neither reply's parent (<root@x>) is ever fetched, but both name it.
    let (_, one) = fx.add(
        Msg::default()
            .id("r1@x")
            .refs("root@x")
            .subject("Re: Ticket")
            .at(T0),
    );
    let (_, two) = fx.add(
        Msg::default()
            .id("r2@x")
            .refs("root@x")
            .subject("Re: Ticket")
            .at(T0 + 30),
    );
    assert_eq!(two.link, ThreadLink::References);
    assert_eq!(two.thread_id, one.thread_id);
    assert_eq!(fx.thread_count(), 1);
}

#[test]
fn angle_brackets_are_stripped_when_matching_ids() {
    let fx = Fixture::open();
    let (_, parent) = fx.add(Msg::default().id("<a@x>").subject("Bracketed").at(T0));
    let (_, child) = fx.add(
        Msg::default()
            .id("<b@x>")
            .reply_to("<a@x>")
            .subject("Re: Bracketed")
            .at(T0 + 10),
    );
    assert_eq!(child.link, ThreadLink::References);
    assert_eq!(child.thread_id, parent.thread_id);
}

// ---------------------------------------------------------------------------
// Out-of-order arrival
// ---------------------------------------------------------------------------

#[test]
fn out_of_order_arrival_keeps_one_stable_thread() {
    let fx = Fixture::open();
    // The newest reply is fetched first; its parents land afterwards.
    let (c, third) = fx.add(
        Msg::default()
            .id("c@x")
            .refs("a@x b@x")
            .subject("Re: Deploy")
            .at(T0 + 120),
    );
    let expected = third.thread_id;

    let (b, second) = fx.add(
        Msg::default()
            .id("b@x")
            .reply_to("a@x")
            .subject("Re: Deploy")
            .at(T0 + 60),
    );
    let (a, first) = fx.add(Msg::default().id("a@x").subject("Deploy").at(T0));

    assert_eq!(second.thread_id, expected);
    assert_eq!(first.thread_id, expected, "thread id never changes");
    assert!(second.merged.is_empty());
    assert!(first.merged.is_empty());
    assert_eq!(fx.thread_count(), 1);

    let thread = fx.thread(expected);
    assert_eq!(thread.message_count, 3);
    assert_eq!(
        thread.root_message_id,
        Some(a),
        "the late-arriving parent becomes the root"
    );
    assert_eq!(thread.last_message_at, Some(T0 + 120));
    assert_eq!(fx.thread_of(b), Some(expected));
    assert_eq!(fx.thread_of(c), Some(expected));
}

#[test]
fn late_message_merges_two_threads_into_the_older_id() {
    let fx = Fixture::open();
    let (a, one) = fx.add(Msg::default().id("a@x").subject("Budget").at(T0));
    let (b, two) = fx.add(Msg::default().id("b@x").subject("Headcount").at(T0 + 10));
    assert_ne!(one.thread_id, two.thread_id, "distinct subjects, no refs");

    // A reply that references both proves they were one conversation.
    let (c, third) = fx.add(
        Msg::default()
            .id("c@x")
            .refs("a@x b@x")
            .subject("Re: Budget")
            .at(T0 + 20),
    );

    let (survivor, absorbed) = (
        one.thread_id.min(two.thread_id),
        one.thread_id.max(two.thread_id),
    );
    assert_eq!(third.thread_id, survivor, "the older thread id survives");
    assert_eq!(third.merged, vec![absorbed]);
    assert_eq!(fx.thread_count(), 1);

    for message in [a, b, c] {
        assert_eq!(fx.thread_of(message), Some(survivor));
    }
    let thread = fx.thread(survivor);
    assert_eq!(thread.message_count, 3);
    assert_eq!(thread.root_message_id, Some(a));

    // Every ref -- including the absorbed thread's -- now points at the survivor.
    let dangling: i64 = fx
        .db
        .with_read(|c| {
            c.query_row(
                "SELECT count(*) FROM thread_refs WHERE thread_id != ?1",
                [survivor],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(dangling, 0);
    assert_eq!(fx.ref_count(), 3, "a@x, b@x and c@x");
}

// ---------------------------------------------------------------------------
// Subject fallback
// ---------------------------------------------------------------------------

#[test]
fn subject_fallback_joins_a_reply_that_lost_its_references() {
    let fx = Fixture::open();
    let (_, one) = fx.add(
        Msg::default()
            .id("a@x")
            .subject("Quarterly report")
            .from("alice@x")
            .at(T0),
    );
    let (_, two) = fx.add(
        Msg::default()
            .id("b@x")
            .subject("Re: Quarterly report")
            .from("bob@x")
            .at(T0 + 3600),
    );
    assert_eq!(two.link, ThreadLink::Subject);
    assert_eq!(two.thread_id, one.thread_id);
    assert_eq!(fx.thread(one.thread_id).message_count, 2);
}

#[test]
fn unrelated_mail_sharing_a_subject_stays_separate() {
    let fx = Fixture::open();
    let (_, one) = fx.add(Msg::default().id("a@x").subject("Invoice").at(T0));
    let (_, two) = fx.add(Msg::default().id("b@x").subject("Invoice").at(T0 + 60));
    assert_eq!(two.link, ThreadLink::New, "neither presents as a reply");
    assert_ne!(two.thread_id, one.thread_id);
    assert_eq!(fx.thread_count(), 2);
}

#[test]
fn subject_fallback_respects_the_time_window() {
    let fx = Fixture::open();
    let (_, one) = fx.add(Msg::default().id("a@x").subject("Status").at(T0));
    let (_, stale) = fx.add(
        Msg::default()
            .id("b@x")
            .subject("Re: Status")
            .at(T0 + SUBJECT_FALLBACK_WINDOW_SECS + 1),
    );
    assert_eq!(stale.link, ThreadLink::New);
    assert_ne!(stale.thread_id, one.thread_id);
}

#[test]
fn references_beat_a_matching_subject() {
    let fx = Fixture::open();
    let (_, decoy) = fx.add(Msg::default().id("a@x").subject("Sync").at(T0));
    let (_, real) = fx.add(Msg::default().id("b@x").subject("Standup").at(T0 + 10));
    // Subject says "Sync", references say otherwise: references win.
    let (_, reply) = fx.add(
        Msg::default()
            .id("c@x")
            .reply_to("b@x")
            .subject("Re: Sync")
            .at(T0 + 20),
    );
    assert_eq!(reply.link, ThreadLink::References);
    assert_eq!(reply.thread_id, real.thread_id);
    assert_ne!(reply.thread_id, decoy.thread_id);
}

// ---------------------------------------------------------------------------
// Scoping, idempotency, aggregates
// ---------------------------------------------------------------------------

#[test]
fn threads_do_not_cross_accounts() {
    let fx = Fixture::open();
    let (_, mine) = fx.add(Msg::default().id("a@x").subject("Shared").at(T0));
    let (other_account, other_mailbox) = fx.add_account("Work");
    let (_, theirs) = fx.add_in(
        other_account,
        other_mailbox,
        Msg::default()
            .id("b@x")
            .reply_to("a@x")
            .subject("Re: Shared")
            .at(T0 + 10),
    );
    assert_ne!(theirs.thread_id, mine.thread_id);
    assert_eq!(fx.thread_count(), 2);
}

#[test]
fn the_same_message_in_two_mailboxes_shares_a_thread() {
    let fx = Fixture::open();
    let archive = fx.add_mailbox("Archive");
    let (_, inbox_copy) = fx.add(Msg::default().id("a@x").subject("Filed").at(T0));
    let (_, archive_copy) = fx.add_in(
        fx.account_id,
        archive,
        Msg::default().id("a@x").subject("Filed").at(T0),
    );
    assert_eq!(archive_copy.thread_id, inbox_copy.thread_id);
    assert_eq!(fx.thread_count(), 1);
}

#[test]
fn rethreading_a_message_is_idempotent() {
    let fx = Fixture::open();
    let (id, first) = fx.add(
        Msg::default()
            .id("a@x")
            .subject("Notes")
            .from("alice@x")
            .at(T0),
    );
    let again = fx
        .db
        .with_write(|c| assign_thread(c, id))
        .unwrap()
        .expect("message exists");
    assert_eq!(again.thread_id, first.thread_id);
    assert!(again.merged.is_empty());
    assert_eq!(fx.thread_count(), 1);
    assert_eq!(fx.thread(first.thread_id).message_count, 1);
}

#[test]
fn a_message_without_any_ids_keeps_its_thread_on_rethread() {
    let fx = Fixture::open();
    // No Message-ID, no references, no reply prefix: nothing to match on.
    let (id, first) = fx.add(Msg::default().subject("Scanned document").at(T0));
    assert_eq!(first.link, ThreadLink::New);

    let again = fx
        .db
        .with_write(|c| assign_thread(c, id))
        .unwrap()
        .expect("message exists");
    assert_eq!(again.link, ThreadLink::Existing);
    assert_eq!(again.thread_id, first.thread_id);
    assert_eq!(fx.thread_count(), 1, "no orphaned duplicate thread");
    assert_eq!(fx.ref_count(), 0, "nothing to register");
}

#[test]
fn threading_a_missing_message_returns_none() {
    let fx = Fixture::open();
    let missing = fx.db.with_write(|c| assign_thread(c, 9_999)).unwrap();
    assert!(missing.is_none());
}

#[test]
fn undated_mail_still_threads_and_never_wins_the_root() {
    let fx = Fixture::open();
    let (dated, first) = fx.add(Msg::default().id("a@x").subject("Ping").at(T0));
    let (_, undated) = fx.add(Msg::default().id("b@x").reply_to("a@x").subject("Re: Ping"));
    assert_eq!(undated.thread_id, first.thread_id);

    let thread = fx.thread(first.thread_id);
    assert_eq!(thread.message_count, 2);
    assert_eq!(
        thread.root_message_id,
        Some(dated),
        "undated mail sorts last, so the dated message roots the thread"
    );
    assert_eq!(thread.last_message_at, Some(T0));
}

#[test]
fn list_threads_orders_by_most_recent_activity() {
    let fx = Fixture::open();
    let (_, older) = fx.add(Msg::default().id("a@x").subject("Older").at(T0));
    let (_, newer) = fx.add(Msg::default().id("b@x").subject("Newer").at(T0 + 1000));

    let threads = fx
        .db
        .with_read(|c| repo::list_threads(c, fx.account_id, 10))
        .unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].id, newer.thread_id);
    assert_eq!(threads[1].id, older.thread_id);
}

// ---------------------------------------------------------------------------
// Subject-fallback guards: the ways a loose subject rule destroys a mailbox
// ---------------------------------------------------------------------------

#[test]
fn a_subject_linked_thread_cannot_drift_indefinitely() {
    // The window is anchored on each thread's FIRST message. Anchored on its
    // last, it would slide forward with every arrival: a year of unrelated
    // "Re: Invoice" mail, each within 30 days of the one before, would chain
    // into a single endless conversation.
    let fx = Fixture::open();
    let step = 25 * 24 * 60 * 60; // 25 days: inside the window, so hops chain
    let hops = 12; // spanning 275 days
    let ids: Vec<String> = (0..hops).map(|n| format!("invoice-{n}@x")).collect();

    let (_, first) = fx.add(Msg::default().id(&ids[0]).subject("Invoice").at(T0));
    for (n, id) in ids.iter().enumerate().skip(1) {
        let at = T0 + step * i64::try_from(n).unwrap();
        fx.add(Msg::default().id(id).subject("Re: Invoice").at(at));
    }

    let threads = fx
        .db
        .with_read(|c| repo::list_threads(c, fx.account_id, 100))
        .unwrap();
    assert!(
        threads.len() > 1,
        "275 days of same-subject mail must not be one conversation"
    );
    for thread in &threads {
        let (first_at, last_at) = (
            thread.first_message_at.unwrap_or_default(),
            thread.last_message_at.unwrap_or_default(),
        );
        assert!(
            last_at - first_at <= SUBJECT_FALLBACK_WINDOW_SECS,
            "thread {} spans {}s, beyond the fallback window",
            thread.id,
            last_at - first_at
        );
    }
    // The genuinely-adjacent first pair still threads together.
    assert_eq!(fx.thread(first.thread_id).message_count, 2);
}

#[test]
fn a_forward_does_not_join_the_conversation_it_quotes() {
    // A forward has a fresh Message-ID and no References, so it always reaches
    // the subject fallback. Joining it would leak its new audience into the
    // original thread's participant set.
    let fx = Fixture::open();
    let (_, original) = fx.add(
        Msg::default()
            .id("a@x")
            .subject("Quarterly report")
            .from("alice@corp.com")
            .to("bob@corp.com")
            .at(T0),
    );
    let (_, forwarded) = fx.add(
        Msg::default()
            .id("b@x")
            .subject("Fwd: Quarterly report")
            .from("alice@corp.com")
            .to("auditor@external.com")
            .at(T0 + 60),
    );

    assert_eq!(forwarded.link, ThreadLink::New);
    assert_ne!(forwarded.thread_id, original.thread_id);
    assert_eq!(
        fx.thread(original.thread_id).participant_list(),
        vec!["alice@corp.com", "bob@corp.com"],
        "the outsider never enters the original thread"
    );
}

#[test]
fn empty_message_ids_do_not_collapse_unrelated_mail() {
    // `<>` and whitespace normalize to an empty id. Registering that as a ref
    // would make every such message share one thread.
    let fx = Fixture::open();
    let (_, one) = fx.add(Msg::default().id("").subject("Scan 001").at(T0));
    let (_, two) = fx.add(Msg::default().id("<>").subject("Payslip").at(T0 + 60));
    let (_, three) = fx.add(Msg::default().id("  ").subject("Photo").at(T0 + 120));

    assert_ne!(one.thread_id, two.thread_id);
    assert_ne!(two.thread_id, three.thread_id);
    assert_eq!(fx.thread_count(), 3);
    assert_eq!(fx.ref_count(), 0, "an empty id is never registered");
}

#[test]
fn rethreading_moves_only_the_message_not_its_old_thread() {
    let fx = Fixture::open();
    // Alpha thread: a root plus a reply linked by subject.
    let (_, alpha) = fx.add(Msg::default().id("a1@x").subject("Alpha").at(T0));
    let (moving, joined) = fx.add(Msg::default().id("a2@x").subject("Re: Alpha").at(T0 + 60));
    assert_eq!(joined.thread_id, alpha.thread_id);
    // A separate conversation.
    let (_, beta) = fx.add(Msg::default().id("b1@x").subject("Beta").at(T0 + 120));

    // Correct the reply's parent to point at Beta, then re-thread it.
    fx.db
        .with_write(|c| {
            c.execute(
                "UPDATE messages SET in_reply_to = 'b1@x' WHERE id = ?1",
                [moving],
            )
        })
        .unwrap();
    let again = fx
        .db
        .with_write(|c| assign_thread(c, moving))
        .unwrap()
        .expect("message exists");

    assert_eq!(again.thread_id, beta.thread_id, "it follows its reference");
    assert!(
        again.merged.is_empty(),
        "one message changing threads is no evidence the old thread's other \
         members belong to the new one"
    );
    assert_eq!(
        fx.thread(alpha.thread_id).message_count,
        1,
        "Alpha keeps its root"
    );
    assert_eq!(fx.thread(beta.thread_id).message_count, 2);
}

#[test]
fn a_thread_emptied_by_rethreading_is_collected() {
    let fx = Fixture::open();
    let (root, alpha) = fx.add(Msg::default().id("a@x").subject("Alpha").at(T0));
    let (_, beta) = fx.add(Msg::default().id("b@x").subject("Beta").at(T0 + 60));
    assert_eq!(fx.thread_count(), 2);

    fx.db
        .with_write(|c| {
            c.execute(
                "UPDATE messages SET in_reply_to = 'b@x' WHERE id = ?1",
                [root],
            )
        })
        .unwrap();
    let moved = fx
        .db
        .with_write(|c| assign_thread(c, root))
        .unwrap()
        .expect("message exists");

    assert_eq!(moved.thread_id, beta.thread_id);
    assert_eq!(fx.thread_count(), 1, "the emptied thread is dropped");
    assert!(
        fx.db
            .with_read(|c| repo::get_thread(c, alpha.thread_id))
            .unwrap()
            .is_none(),
        "Alpha no longer exists"
    );
    assert_eq!(
        moved.merged,
        vec![alpha.thread_id],
        "a thread id that ceases to exist must be reported, or a follower \
         cannot tell 'gone' from 'still there'"
    );
}

#[test]
fn a_collected_thread_hands_its_phantom_refs_to_the_thread_it_folds_into() {
    let fx = Fixture::open();
    // P is the only member of its thread and holds a phantom ref to <root@x>.
    let (p, alpha) = fx.add(
        Msg::default()
            .id("p@x")
            .refs("root@x")
            .subject("Re: Ticket")
            .at(T0),
    );
    let (_, beta) = fx.add(Msg::default().id("q@x").subject("Other").at(T0 + 60));

    // Re-point P at the other conversation, dropping its old reference chain
    // entirely (keeping it would legitimately merge the two threads instead).
    // P's old thread now empties out, still holding the <root@x> phantom.
    fx.db
        .with_write(|c| {
            c.execute(
                "UPDATE messages SET in_reply_to = 'q@x', references_hdr = NULL WHERE id = ?1",
                [p],
            )
        })
        .unwrap();
    let moved = fx
        .db
        .with_write(|c| assign_thread(c, p))
        .unwrap()
        .expect("message exists");
    assert_eq!(moved.thread_id, beta.thread_id);
    assert!(fx
        .db
        .with_read(|c| repo::get_thread(c, alpha.thread_id))
        .unwrap()
        .is_none());

    // A sibling naming the same phantom parent must still link by reference:
    // deleting the emptied thread outright would have cascaded that ref away.
    let (_, sibling) = fx.add(
        Msg::default()
            .id("s@x")
            .refs("root@x")
            .subject("Re: Ticket")
            .at(T0 + 120),
    );
    assert_eq!(sibling.link, ThreadLink::References);
    assert_eq!(sibling.thread_id, beta.thread_id);
}

#[test]
fn the_subject_fallback_never_moves_an_already_threaded_message() {
    // The fallback is the weakest signal. Re-running it on a message that
    // already has a conversation would let a newer same-subject thread quietly
    // steal it — and `assign_thread` promises idempotency.
    let fx = Fixture::open();
    let day = 24 * 60 * 60;
    let (_, one) = fx.add(Msg::default().id("a@x").subject("Invoice").at(T0));
    let (b, joined) = fx.add(
        Msg::default()
            .id("b@x")
            .subject("Re: Invoice")
            .at(T0 + 5 * day),
    );
    assert_eq!(joined.link, ThreadLink::Subject);
    assert_eq!(joined.thread_id, one.thread_id);

    // A second, more recent conversation with the same subject.
    let (_, two) = fx.add(
        Msg::default()
            .id("c@x")
            .subject("Invoice")
            .at(T0 + 10 * day),
    );
    let (_, also) = fx.add(
        Msg::default()
            .id("d@x")
            .subject("Re: Invoice")
            .at(T0 + 12 * day),
    );
    assert_eq!(also.thread_id, two.thread_id);
    assert_ne!(two.thread_id, one.thread_id);

    let again = fx
        .db
        .with_write(|c| assign_thread(c, b))
        .unwrap()
        .expect("message exists");
    assert_eq!(
        again.thread_id, one.thread_id,
        "B stays where it is; the newer same-subject thread must not take it"
    );
    assert!(again.merged.is_empty());
    assert_eq!(fx.thread(one.thread_id).message_count, 2);
    assert_eq!(fx.thread(two.thread_id).message_count, 2);
}

// ---------------------------------------------------------------------------
// Aggregates & repair
// ---------------------------------------------------------------------------

#[test]
fn participants_include_cc_and_span_the_whole_thread() {
    let fx = Fixture::open();
    let (_, thread) = fx.add(
        Msg::default()
            .id("a@x")
            .subject("Launch")
            .from("Alice@Corp.com")
            .to("bob@corp.com")
            .cc("carol@corp.com, dave@corp.com")
            .at(T0),
    );
    fx.add(
        Msg::default()
            .id("b@x")
            .reply_to("a@x")
            .subject("Re: Launch")
            .from("bob@corp.com")
            .to("alice@corp.com")
            .cc("erin@corp.com")
            .at(T0 + 60),
    );

    assert_eq!(
        fx.thread(thread.thread_id).participant_list(),
        vec![
            "alice@corp.com",
            "bob@corp.com",
            "carol@corp.com",
            "dave@corp.com",
            "erin@corp.com"
        ],
    );
}

#[test]
fn recompute_zeroes_a_thread_that_lost_every_message() {
    let fx = Fixture::open();
    let (id, thread) = fx.add(
        Msg::default()
            .id("a@x")
            .subject("Ephemeral")
            .from("alice@x")
            .at(T0),
    );
    fx.db
        .with_write(|c| c.execute("DELETE FROM messages WHERE id = ?1", [id]))
        .unwrap();

    fx.db
        .with_write(|c| recompute_thread(c, thread.thread_id))
        .unwrap();

    let empty = fx.thread(thread.thread_id);
    assert_eq!(empty.message_count, 0);
    assert_eq!(empty.root_message_id, None);
    assert_eq!(empty.first_message_at, None);
    assert_eq!(empty.last_message_at, None);
    assert_eq!(empty.participants, None);
}

#[test]
fn backfill_threads_messages_left_without_one() {
    let fx = Fixture::open();
    let (a, _) = fx.add(Msg::default().id("a@x").subject("Backfill").at(T0));
    let (b, _) = fx.add(
        Msg::default()
            .id("b@x")
            .reply_to("a@x")
            .subject("Re: Backfill")
            .at(T0 + 60),
    );
    // Simulate rows stored before threading existed.
    fx.db
        .with_write(|c| {
            c.execute("UPDATE messages SET thread_id = NULL", [])?;
            c.execute("DELETE FROM thread_refs", [])?;
            c.execute("DELETE FROM threads", [])
        })
        .unwrap();
    assert_eq!(fx.thread_count(), 0);

    let threaded = fx
        .db
        .with_write(|c| thread_unthreaded_messages(c, 100))
        .unwrap();
    assert_eq!(threaded, 2);
    assert_eq!(fx.thread_count(), 1, "the chain is rebuilt, not split");
    assert_eq!(fx.thread_of(a), fx.thread_of(b));

    // Idempotent: nothing left to do.
    let again = fx
        .db
        .with_write(|c| thread_unthreaded_messages(c, 100))
        .unwrap();
    assert_eq!(again, 0);
}

#[test]
fn the_conversation_list_uses_its_index() {
    // Keep this query in step with `repo::list_threads` — it exists to prove
    // that shape is indexed, and silently drifts useless if the two diverge.
    let fx = Fixture::open();
    let plan: Vec<String> = fx
        .db
        .with_read(|c| {
            let mut stmt = c.prepare(
                "EXPLAIN QUERY PLAN
                 SELECT id FROM threads WHERE account_id = 1
                 ORDER BY last_message_at DESC LIMIT 20",
            )?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>("detail"))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    let joined = plan.join(" | ");
    assert!(
        joined.contains("idx_threads_account_activity"),
        "conversation list should use the account+activity index, plan was: {joined}"
    );
    assert!(
        !joined.to_uppercase().contains("TEMP B-TREE"),
        "conversation list must not sort every thread in the account, plan was: {joined}"
    );
}

// ---------------------------------------------------------------------------
// Subject normalization
// ---------------------------------------------------------------------------

mod subject {
    use super::{normalize_subject, SubjectPrefix};

    #[track_caller]
    fn norm(subject: &str) -> (String, SubjectPrefix) {
        normalize_subject(Some(subject))
    }

    #[test]
    fn strips_reply_prefixes() {
        for raw in [
            "Re: Hello",
            "RE: Hello",
            "re:Hello",
            "Re: Re: Re: Hello",
            "Re[2]: Hello",
            "Re(3): Hello",
            "AW: Hello",
            "SV: Hello",
            "Odp: Hello",
        ] {
            let (text, prefix) = norm(raw);
            assert_eq!(text, "hello", "normalizing {raw:?}");
            assert_eq!(prefix, SubjectPrefix::Reply, "{raw:?} is a reply");
        }
    }

    #[test]
    fn strips_forward_prefixes_without_calling_them_replies() {
        for raw in [
            "Fwd: Hello",
            "FW: Hello",
            "fwd:Hello",
            "WG: Hello",
            "Hello (fwd)",
            "Fwd: Re: Hello",
        ] {
            let (text, prefix) = norm(raw);
            assert_eq!(text, "hello", "normalizing {raw:?}");
            assert_eq!(
                prefix,
                SubjectPrefix::Forward,
                "{raw:?} is a forward, not a reply — the outermost prefix wins"
            );
        }
        // Inverted nesting: a reply to a forward continues that conversation.
        assert_eq!(
            norm("Re: Fwd: Hello"),
            ("hello".to_owned(), SubjectPrefix::Reply)
        );
    }

    #[test]
    fn plain_subjects_carry_no_prefix() {
        for raw in ["Hello", "Reminder: standup", "Research notes", "Recipe"] {
            let (_, prefix) = norm(raw);
            assert_eq!(prefix, SubjectPrefix::None, "{raw:?} has no prefix");
        }
        assert_eq!(norm("Reminder: standup").0, "reminder: standup");
        assert_eq!(norm("Research notes").0, "research notes");
    }

    #[test]
    fn strips_leading_list_tags() {
        assert_eq!(norm("[rust-dev] Hello").0, "hello");
        assert_eq!(norm("Re: [rust-dev] Hello").0, "hello");
        assert_eq!(
            norm("[rust-dev] Re: Hello"),
            ("hello".to_owned(), SubjectPrefix::Reply)
        );
    }

    #[test]
    fn collapses_whitespace_and_case() {
        assert_eq!(norm("  Weekly   Sync\tNotes  ").0, "weekly sync notes");
    }

    #[test]
    fn handles_degenerate_input() {
        assert_eq!(
            normalize_subject(None),
            (String::new(), SubjectPrefix::None)
        );
        assert_eq!(norm("").0, "");
        assert_eq!(norm("Re:").0, "");
        assert_eq!(norm("Re").0, "re", "a bare prefix without a colon is text");
        assert_eq!(norm("[unclosed Hello").0, "[unclosed hello");
        // Pathological prefix stacking must terminate, not spin.
        let deep = "Re: ".repeat(200) + "end";
        assert_eq!(norm(&deep).1, SubjectPrefix::Reply);
    }

    #[test]
    fn preserves_non_ascii_subjects() {
        assert_eq!(norm("Re: Grüße aus München").0, "grüße aus münchen");
        assert_eq!(norm("Re: 会議の件").0, "会議の件");
    }
}
