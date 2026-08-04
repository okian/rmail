//! Delta sync against servers that can say *what changed*: QRESYNC (RFC 7162)
//! and CONDSTORE.
//!
//! Every test here follows the same shape — sync a folder, change the server
//! underneath it, sync again — and asserts on the *traffic* as much as on the
//! rows, because a delta that produces the right database by re-downloading the
//! mailbox is not a delta.

use crate::imap::mock::{MockConfig, MockImap};
use crate::imap::ImapCapabilities;
use crate::repo;
use crate::sync::DeltaStrategy;
use tokio_util::sync::CancellationToken;

use super::harness::{body_fetches, commands_starting, mock_config, raw, Fixture, UIDVALIDITY};

/// A server that advertises CONDSTORE but not QRESYNC.
fn condstore_only(config: MockConfig) -> MockConfig {
    config.capabilities(&["IMAP4rev1", "IDLE", "CONDSTORE"])
}

/// Sync a folder to the point where it has a modseq baseline: the initial walk
/// downloads it, then one delta records where the server's modseq stood.
async fn synced_with_baseline(fx: &Fixture, mock: &MockImap) {
    fx.full_sync(mock).await;
    let first = fx.delta(mock).await;
    assert_eq!(
        first.strategy,
        DeltaStrategy::UidDiff,
        "a folder the walk built has no modseq baseline yet, so the first \
         delta has to enumerate before it can checkpoint one"
    );
    assert!(fx.sync_state().highestmodseq.is_some());
}

#[tokio::test]
async fn an_unchanged_folder_transfers_nothing() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(5)).await;
    synced_with_baseline(&fx, &mock).await;

    let quiet = MockImap::start(mock_config(5)).await;
    let report = fx.delta(&quiet).await;

    assert_eq!(report.strategy, DeltaStrategy::Qresync);
    assert!(
        report.unchanged,
        "the server's modseq matched the checkpoint"
    );
    assert_eq!(report.new_messages, 0);
    assert_eq!(report.flag_updates, 0);
    assert_eq!(report.expunged, 0);
    assert!(
        commands_starting(&quiet, "UID FETCH").is_empty(),
        "a folder that did not change must not be fetched at all: {:?}",
        quiet.commands()
    );
    assert!(
        commands_starting(&quiet, "UID SEARCH").is_empty(),
        "nor enumerated"
    );
}

#[tokio::test]
async fn the_probe_asks_changedsince_the_stored_modseq() {
    // "Only changes transfer" lives or dies on this one command. Asserting on
    // the *effects* is not enough: with `CHANGEDSINCE 0` the server returns
    // every message, `replace_flags` finds the unchanged ones identical, and
    // every count in every other test comes out the same while the folder is
    // re-read end to end on each sync.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    synced_with_baseline(&fx, &mock).await;
    assert_eq!(fx.sync_state().highestmodseq, Some(1));

    let changed = MockImap::start(mock_config(3).change(2, &["\\Flagged"], 9)).await;
    fx.delta(&changed).await;

    let probes = commands_starting(&changed, "UID FETCH");
    assert_eq!(
        probes,
        vec!["UID FETCH 1:* (UID FLAGS MODSEQ) (CHANGEDSINCE 1 VANISHED)"],
        "one probe, scoped to what happened after the checkpoint"
    );

    // And the checkpoint the next run asks from is the one this run recorded.
    let again = MockImap::start(mock_config(3).change(2, &["\\Flagged"], 9)).await;
    fx.delta(&again).await;
    assert_eq!(
        commands_starting(&again, "UID FETCH"),
        Vec::<String>::new(),
        "modseq 9 is now the checkpoint, and nothing has happened since"
    );
}

#[tokio::test]
async fn the_select_reported_state_is_persisted_on_the_mailbox_row() {
    // First acceptance criterion: per-folder UIDVALIDITY *and* HIGHESTMODSEQ.
    // `sync_state` holds the applied checkpoint; the `mailboxes` row mirrors
    // what the server last said, alongside UIDVALIDITY/UIDNEXT.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(4).change(4, &["\\Seen"], 17)).await;
    synced_with_baseline(&fx, &mock).await;

    let row = fx.mailbox_row();
    assert_eq!(row.uidvalidity, Some(UIDVALIDITY));
    assert_eq!(row.uidnext, Some(5));
    assert_eq!(row.highestmodseq, Some(17));
}

#[tokio::test]
async fn a_flag_change_arrives_without_the_body() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    synced_with_baseline(&fx, &mock).await;
    assert_eq!(fx.flags_of(2), vec!["\\Seen".to_owned()]);

    // The user stars message 2 on their phone: same body, new flags, new
    // modseq.
    let changed = MockImap::start(mock_config(3).change(2, &["\\Seen", "\\Flagged"], 9)).await;
    let report = fx.delta(&changed).await;

    assert_eq!(report.strategy, DeltaStrategy::Qresync);
    assert!(!report.unchanged);
    assert_eq!(report.flag_updates, 1, "exactly the one message changed");
    assert_eq!(report.new_messages, 0);
    assert_eq!(
        fx.flags_of(2),
        vec!["\\Flagged".to_owned(), "\\Seen".to_owned()]
    );
    assert_eq!(fx.flags_of(1), vec!["\\Seen".to_owned()], "untouched");
    assert!(
        body_fetches(&changed).is_empty(),
        "a flag flip must not re-download a message: {:?}",
        changed.commands()
    );
    assert_eq!(report.highestmodseq, Some(9), "the checkpoint advanced");
}

#[tokio::test]
async fn a_flag_the_server_dropped_is_cleared_locally() {
    // IMAP flags are a set the server owns outright — a delta that only ever
    // adds would leave a message marked \Seen forever after it was marked
    // unread elsewhere.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(2)).await;
    synced_with_baseline(&fx, &mock).await;
    assert_eq!(fx.flags_of(1), vec!["\\Seen".to_owned()]);

    let unread = MockImap::start(mock_config(2).change(1, &[], 4)).await;
    let report = fx.delta(&unread).await;

    assert_eq!(report.flag_updates, 1);
    assert!(
        fx.flags_of(1).is_empty(),
        "marking a message unread elsewhere clears the local flag"
    );
}

#[tokio::test]
async fn an_expunge_is_reported_as_vanished_and_applied() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    synced_with_baseline(&fx, &mock).await;
    assert_eq!(fx.stored_uids(), vec![1, 2, 3]);
    assert_eq!(fx.thread_count(), 3);

    // Message 2 is deleted elsewhere. UIDNEXT does not move backwards, so the
    // hole is permanent.
    let after = MockImap::start(
        MockConfig::default()
            .password("pw")
            .uidvalidity(u32::try_from(UIDVALIDITY).unwrap())
            .fetch(1, &["\\Seen"], &raw(1))
            .fetch(3, &["\\Seen"], &raw(3))
            .expunged(2, 7),
    )
    .await;
    let report = fx.delta(&after).await;

    assert_eq!(report.strategy, DeltaStrategy::Qresync);
    assert_eq!(report.expunged, 1);
    assert_eq!(fx.stored_uids(), vec![1, 3], "the expunged row is gone");
    assert_eq!(
        fx.thread_count(),
        2,
        "and the conversation it was alone in was collected with it"
    );
    assert!(
        commands_starting(&after, "UID SEARCH").is_empty(),
        "QRESYNC reports expunges directly; enumerating would be a wasted \
         round trip: {:?}",
        after.commands()
    );
    assert!(
        body_fetches(&after).is_empty(),
        "and nothing was re-downloaded"
    );
}

#[tokio::test]
async fn new_mail_is_downloaded_by_the_delta() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    synced_with_baseline(&fx, &mock).await;

    // Two new messages land, at modseqs above the checkpoint.
    let mut grown = mock_config(3);
    for uid in 4..=5u32 {
        grown = grown.fetch_at("INBOX", uid, &["\\Recent"], &raw(uid), 12);
    }
    let grown = MockImap::start(grown).await;
    let report = fx.delta(&grown).await;

    assert_eq!(report.new_messages, 2);
    assert_eq!(fx.stored_uids(), vec![1, 2, 3, 4, 5]);
    assert_eq!(
        body_fetches(&grown).len(),
        1,
        "one body fetch, for the two new UIDs only: {:?}",
        body_fetches(&grown)
    );
    assert!(
        body_fetches(&grown)[0].contains("4:5"),
        "and it asked for exactly those: {:?}",
        body_fetches(&grown)
    );
    assert_eq!(
        fx.sync_state().last_synced_uid,
        Some(5),
        "the high-water mark moved to the new ceiling"
    );
}

#[tokio::test]
async fn a_condstore_server_finds_expunges_by_enumeration() {
    // CONDSTORE says what changed but never what left, so the live UID set has
    // to be asked for separately.
    let fx = Fixture::open().await;
    let mock = MockImap::start(condstore_only(mock_config(3))).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;

    let after = MockImap::start(condstore_only(
        MockConfig::default()
            .password("pw")
            .uidvalidity(u32::try_from(UIDVALIDITY).unwrap())
            .fetch(1, &["\\Seen"], &raw(1))
            .fetch(3, &["\\Seen"], &raw(3))
            .expunged(2, 7),
    ))
    .await;
    let report = fx.delta(&after).await;

    assert_eq!(report.strategy, DeltaStrategy::Condstore);
    assert_eq!(report.expunged, 1);
    assert_eq!(fx.stored_uids(), vec![1, 3]);
    assert_eq!(
        commands_starting(&after, "UID SEARCH").len(),
        1,
        "exactly one enumeration"
    );
    assert!(
        !after
            .commands()
            .iter()
            .any(|c| c.to_ascii_uppercase().contains("VANISHED")),
        "VANISHED is a QRESYNC modifier; a CONDSTORE-only server must not be \
         asked for it: {:?}",
        after.commands()
    );
}

#[tokio::test]
async fn a_server_that_refuses_enable_downgrades_to_condstore() {
    // Advertising an extension and then rejecting it is a real server
    // behaviour; it must cost a round trip, not the sync.
    let fx = Fixture::open().await;
    let mock = MockImap::start(condstore_only(mock_config(2))).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;

    let claimed = ImapCapabilities {
        idle: true,
        condstore: true,
        qresync: true,
        move_: false,
    };
    let changed = MockImap::start(condstore_only(mock_config(2).change(
        1,
        &["\\Seen", "\\Flagged"],
        8,
    )))
    .await;
    let report = fx.delta_claiming(&changed, claimed).await;

    assert_eq!(
        report.strategy,
        DeltaStrategy::Condstore,
        "the refusal downgraded the strategy instead of failing the sync"
    );
    assert_eq!(report.flag_updates, 1, "and the delta still worked");
    assert_eq!(
        commands_starting(&changed, "ENABLE").len(),
        1,
        "it asked once and did not retry"
    );
}

#[tokio::test]
async fn a_cancelled_run_leaves_the_checkpoint_where_it_was() {
    // The modseq is a record of what has been *applied*. Advancing it past a
    // change that was never applied loses that change permanently, so a run
    // that stops early must not move it.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    synced_with_baseline(&fx, &mock).await;
    let checkpoint = fx.sync_state();

    let changed = MockImap::start(mock_config(3).change(1, &["\\Seen", "\\Flagged"], 20)).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let report = fx.delta_with(&changed, &cancel).await;

    assert!(report.cancelled);
    assert_eq!(
        fx.sync_state().highestmodseq,
        checkpoint.highestmodseq,
        "the checkpoint did not skip past the unapplied change"
    );
    assert_eq!(
        fx.flags_of(1),
        vec!["\\Seen".to_owned()],
        "and nothing was applied"
    );

    // The next run picks the change up.
    let report = fx.delta(&changed).await;
    assert!(!report.cancelled);
    assert_eq!(report.flag_updates, 1);
    assert_eq!(fx.sync_state().highestmodseq, Some(20));
}

#[tokio::test]
async fn a_uidvalidity_bump_triggers_a_safe_resync() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    synced_with_baseline(&fx, &mock).await;
    assert_eq!(fx.message_count(), 3);

    // The server re-keys the UID space: the old UIDs now address different
    // messages, so nothing local is addressable any more.
    let rekeyed = MockImap::start(mock_config(3).uidvalidity(7)).await;
    let report = fx.delta(&rekeyed).await;

    assert_eq!(report.strategy, DeltaStrategy::Full);
    assert!(report.resynced);
    assert_eq!(report.purged_stale, 3, "the stale UID space was dropped");
    assert_eq!(report.new_messages, 3, "and the new one downloaded");
    assert_eq!(
        fx.message_count(),
        3,
        "the folder is replaced, not shown twice"
    );
    assert!(fx.stored_uids().is_empty(), "nothing left in the old space");
    assert_eq!(fx.uids_at(7), vec![1, 2, 3]);

    let state = fx.sync_state();
    assert_eq!(state.uidvalidity, Some(7));
    assert!(state.full_sync_done);
    assert!(
        state.highestmodseq.is_some(),
        "and the rebuilt folder is delta-syncable again"
    );
}

#[tokio::test]
async fn a_folder_that_was_never_synced_hands_back_to_the_full_walk() {
    // There is no "since" to ask about before the first download, so pretending
    // a delta happened would checkpoint a modseq over an empty folder and lose
    // every message below the ceiling forever.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(4)).await;

    let report = fx.delta(&mock).await;

    assert_eq!(report.strategy, DeltaStrategy::Full);
    assert!(report.resynced);
    assert_eq!(report.purged_stale, 0, "there was nothing stale to purge");
    assert_eq!(report.new_messages, 4);
    assert_eq!(fx.stored_uids(), vec![1, 2, 3, 4]);
    assert!(fx.sync_state().full_sync_done);

    // And from here on it deltas.
    let quiet = MockImap::start(mock_config(4)).await;
    assert_eq!(fx.delta(&quiet).await.strategy, DeltaStrategy::Qresync);
}

#[tokio::test]
async fn a_partially_walked_folder_takes_new_mail_but_not_the_backlog() {
    // A delta asks about the whole UID space, but the backlog below the walk's
    // low-water mark belongs to the walk — downloading it here would undo the
    // newest-first ordering that makes a large mailbox usable early.
    let fx = Fixture::open().await;
    let mailbox_id = fx.mailbox_id;
    let mock = MockImap::start(mock_config(6)).await;
    fx.full_sync(&mock).await;

    // Rewrite the checkpoint as if the walk had only reached UID 4, and drop
    // the rows below it, the way an interrupted walk leaves a folder.
    fx.db
        .write(move |c| {
            c.execute(
                "DELETE FROM messages WHERE mailbox_id = ?1 AND uid < 4",
                [mailbox_id],
            )?;
            repo::upsert_sync_state(
                c,
                &repo::SyncState {
                    mailbox_id,
                    uidvalidity: Some(UIDVALIDITY),
                    highestmodseq: Some(1),
                    last_synced_uid: Some(6),
                    walked_down_to: Some(4),
                    last_sync_at: Some(0),
                    full_sync_done: false,
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(fx.stored_uids(), vec![4, 5, 6]);

    // New mail arrives, and an old message in the untouched backlog changes.
    let grown = MockImap::start(
        mock_config(6)
            .fetch_at("INBOX", 7, &["\\Recent"], &raw(7), 11)
            .change(2, &["\\Seen", "\\Flagged"], 11),
    )
    .await;
    let report = fx.delta(&grown).await;

    assert_eq!(report.new_messages, 1, "only the new arrival");
    assert_eq!(
        fx.stored_uids(),
        vec![4, 5, 6, 7],
        "UID 2 stays the backlog walk's job"
    );
    assert!(
        !fx.sync_state().full_sync_done,
        "and the folder is still incomplete, so the walk will finish it"
    );
}

#[tokio::test]
async fn a_condstore_expunge_that_moves_no_modseq_is_still_found() {
    // RFC 7162 §3.1.2.2 makes expunges bump HIGHESTMODSEQ *for a QRESYNC
    // server*. CONDSTORE alone carries no such promise: a server may track
    // mod-sequences for flag changes only, so an expunge with nothing else
    // going on leaves the modseq exactly where it was. Taking the
    // "nothing changed" shortcut there would hide that message's deletion on
    // this run and on every run afterwards, permanently.
    let fx = Fixture::open().await;
    let mock = MockImap::start(condstore_only(mock_config(3))).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;
    assert_eq!(fx.sync_state().highestmodseq, Some(1));
    assert_eq!(fx.stored_uids(), vec![1, 2, 3]);

    // UID 2 is expunged, and the folder's HIGHESTMODSEQ does not move.
    let after = MockImap::start(condstore_only(
        MockConfig::default()
            .password("pw")
            .uidvalidity(u32::try_from(UIDVALIDITY).unwrap())
            .fetch(1, &["\\Seen"], &raw(1))
            .fetch(3, &["\\Seen"], &raw(3))
            .expunged(2, 0),
    ))
    .await;
    let report = fx.delta(&after).await;

    assert_eq!(report.strategy, DeltaStrategy::Condstore);
    assert!(
        !report.unchanged,
        "a CONDSTORE server's unmoved modseq is not proof the folder is quiet"
    );
    assert_eq!(report.expunged, 1);
    assert_eq!(fx.stored_uids(), vec![1, 3]);
}

#[tokio::test]
async fn a_message_that_changed_and_then_vanished_is_not_downloaded_back() {
    // The probe and the expunge report can disagree: a message that changed and
    // was then deleted shows up in both. Acting on the change alone would
    // download it straight back into the folder it just left, and the next run
    // would delete it again — forever.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    synced_with_baseline(&fx, &mock).await;

    // UID 2 is dropped from the local copy so it looks like a message the
    // folder is missing, while the server reports it both changed and vanished.
    let mailbox_id = fx.mailbox_id;
    fx.db
        .write(move |c| {
            c.execute(
                "DELETE FROM messages WHERE mailbox_id = ?1 AND uid = 2",
                [mailbox_id],
            )
        })
        .await
        .unwrap();
    assert_eq!(fx.stored_uids(), vec![1, 3]);

    let after = MockImap::start(mock_config(3).change(2, &["\\Deleted"], 8).expunged(2, 8)).await;
    let report = fx.delta(&after).await;

    assert_eq!(report.new_messages, 0, "the departed message stayed gone");
    assert_eq!(fx.stored_uids(), vec![1, 3]);
    assert!(
        body_fetches(&after).is_empty(),
        "and no body was requested for it: {:?}",
        after.commands()
    );
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delta_syncing_an_unknown_mailbox_is_not_found() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(1)).await;
    let (mut session, capabilities) = super::harness::connect_with_capabilities(&mock).await;

    let err = crate::sync::delta_sync(
        &mut session,
        &fx.db,
        9_999,
        capabilities,
        crate::sync::SyncOptions::default(),
        &CancellationToken::new(),
    )
    .await
    .expect_err("an unknown mailbox has nothing to sync");
    assert_eq!(
        tonic::Status::from(err).code(),
        tonic::Code::NotFound,
        "an unknown mailbox is NOT_FOUND at the boundary"
    );
}

#[tokio::test]
async fn an_unselectable_folder_is_not_found_not_unauthenticated() {
    // Telling a client its credentials are bad when the folder simply went away
    // sends it chasing the wrong problem.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(1).unselectable("INBOX")).await;
    let (mut session, capabilities) = super::harness::connect_with_capabilities(&mock).await;

    let err = fx
        .delta_on(&mut session, capabilities, &CancellationToken::new())
        .await
        .expect_err("SELECT was refused");
    assert_eq!(tonic::Status::from(err).code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn a_server_without_uid_response_codes_is_unavailable() {
    for config in [
        mock_config(2).without_uidvalidity(),
        mock_config(2).without_uidnext(),
    ] {
        let fx = Fixture::open().await;
        let mock = MockImap::start(config).await;
        let (mut session, capabilities) = super::harness::connect_with_capabilities(&mock).await;

        let err = fx
            .delta_on(&mut session, capabilities, &CancellationToken::new())
            .await
            .expect_err("a delta needs both response codes to key the UID space");
        assert_eq!(tonic::Status::from(err).code(), tonic::Code::Unavailable);
    }
}

#[tokio::test]
async fn a_refused_probe_does_not_look_like_a_quiet_folder() {
    // async-imap's fetch stream stops at the tagged response without inspecting
    // its status, so a server answering `NO [LIMIT]` produces zero items and no
    // error — byte-for-byte what "nothing changed" looks like. Taken at face
    // value the run would checkpoint past changes it never received, and they
    // would never be asked for again.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    synced_with_baseline(&fx, &mock).await;
    assert_eq!(fx.sync_state().highestmodseq, Some(1));

    let busy = MockImap::start(
        mock_config(3)
            .change(1, &["\\Flagged"], 6)
            .refusing_uid_commands(),
    )
    .await;
    let report = fx.delta(&busy).await;

    assert_eq!(report.flag_updates, 0, "nothing was received to apply");
    assert_eq!(
        fx.sync_state().highestmodseq,
        Some(1),
        "so the checkpoint held and the next run asks again"
    );
    assert_eq!(report.highestmodseq, Some(1));

    // And once the server is willing, the change lands.
    let recovered = MockImap::start(mock_config(3).change(1, &["\\Flagged"], 6)).await;
    let report = fx.delta(&recovered).await;
    assert_eq!(report.flag_updates, 1);
    assert_eq!(fx.flags_of(1), vec!["\\Flagged".to_owned()]);
    assert_eq!(fx.sync_state().highestmodseq, Some(6));
}
