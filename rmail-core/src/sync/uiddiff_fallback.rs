//! Delta sync against a server with neither CONDSTORE nor QRESYNC.
//!
//! Plenty of IMAP servers still cannot say what changed. Without a modseq there
//! is no cheap question to ask, so the fallback asks the two expensive-but-
//! bounded ones instead: *which UIDs do you still have* (`UID SEARCH ALL`, one
//! small round trip) and *what are their flags* (a header-only sweep of what is
//! already stored). No message body moves for anything already held.

use crate::imap::mock::{MockConfig, MockImap};
use crate::sync::DeltaStrategy;

use super::harness::{body_fetches, commands_starting, mock_config, raw, Fixture, UIDVALIDITY};

/// A server from before RFC 4551 — no modseq of any kind.
fn legacy(config: MockConfig) -> MockConfig {
    config.capabilities(&["IMAP4rev1", "IDLE"])
}

/// A legacy server holding messages with UIDs `1..=count`.
fn legacy_config(count: u32) -> MockConfig {
    legacy(mock_config(count))
}

/// A legacy server holding exactly `uids`.
fn legacy_with(uids: &[u32]) -> MockConfig {
    let mut config = legacy(
        MockConfig::default()
            .password("pw")
            .uidvalidity(u32::try_from(UIDVALIDITY).unwrap()),
    );
    for uid in uids {
        config = config.fetch(*uid, &["\\Seen"], &raw(*uid));
    }
    config
}

#[tokio::test]
async fn a_server_without_condstore_never_asks_for_a_modseq() {
    // MODSEQ and CHANGEDSINCE are CONDSTORE data items. Sending either to a
    // server that never advertised the extension earns a tagged BAD, so the
    // fallback has to be a different question, not the same one hopefully.
    let fx = Fixture::open().await;
    let mock = MockImap::start(legacy_config(3)).await;
    fx.full_sync(&mock).await;

    let report = fx.delta(&mock).await;

    assert_eq!(report.strategy, DeltaStrategy::UidDiff);
    assert_eq!(
        report.highestmodseq, None,
        "there is no modseq to check point"
    );
    let commands = mock.commands();
    assert!(
        !commands
            .iter()
            .any(|c| c.to_ascii_uppercase().contains("CHANGEDSINCE")),
        "no CHANGEDSINCE: {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|c| c.to_ascii_uppercase().contains("MODSEQ")),
        "no MODSEQ data item: {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|c| c.to_ascii_uppercase().contains("VANISHED")),
        "no VANISHED: {commands:?}"
    );
    assert_eq!(
        commands_starting(&mock, "UID SEARCH").len(),
        1,
        "the enumeration is how this server is asked what it still has"
    );
}

#[tokio::test]
async fn the_diff_detects_an_expunge() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(legacy_config(4)).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;
    assert_eq!(fx.stored_uids(), vec![1, 2, 3, 4]);
    assert_eq!(fx.thread_count(), 4);

    // Two messages are deleted elsewhere. This server cannot report VANISHED,
    // so the only evidence is their absence from the enumeration.
    let after = MockImap::start(legacy_with(&[1, 4])).await;
    let report = fx.delta(&after).await;

    assert_eq!(report.strategy, DeltaStrategy::UidDiff);
    assert_eq!(report.expunged, 2);
    assert_eq!(fx.stored_uids(), vec![1, 4]);
    assert_eq!(fx.message_count(), 2);
    assert_eq!(
        fx.thread_count(),
        2,
        "the conversations they were alone in went with them"
    );
}

#[tokio::test]
async fn the_diff_detects_new_mail() {
    // The flag sweep only ever looks at UIDs already stored, so it can never
    // discover an arrival; the enumeration is what sees one.
    let fx = Fixture::open().await;
    let mock = MockImap::start(legacy_config(2)).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;

    let grown = MockImap::start(legacy_config(4)).await;
    let report = fx.delta(&grown).await;

    assert_eq!(report.new_messages, 2);
    assert_eq!(fx.stored_uids(), vec![1, 2, 3, 4]);
    let bodies = body_fetches(&grown);
    assert_eq!(
        bodies.len(),
        1,
        "one body fetch, covering only the new UIDs: {bodies:?}"
    );
    assert!(bodies[0].contains("3:4"), "{bodies:?}");
    assert_eq!(fx.sync_state().last_synced_uid, Some(4));
}

#[tokio::test]
async fn the_sweep_reconciles_a_flag_change() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(legacy_config(3)).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;
    assert_eq!(fx.flags_of(3), vec!["\\Seen".to_owned()]);

    // No modseq moves — on this server nothing about the change is
    // distinguishable except the flags themselves.
    let changed = MockImap::start(legacy_config(3).change(3, &["\\Answered"], 1)).await;
    let report = fx.delta(&changed).await;

    assert_eq!(report.flag_updates, 1, "only the message that differs");
    assert_eq!(fx.flags_of(3), vec!["\\Answered".to_owned()]);
    assert_eq!(fx.flags_of(1), vec!["\\Seen".to_owned()]);
    assert!(
        body_fetches(&changed).is_empty(),
        "the sweep is header-only: {:?}",
        changed.commands()
    );
}

#[tokio::test]
async fn an_unchanged_folder_still_costs_only_headers() {
    // Without a modseq there is no way to skip the work entirely — but the work
    // must stay proportional to the folder's *identity*, never its bytes.
    let fx = Fixture::open().await;
    let mock = MockImap::start(legacy_config(6)).await;
    fx.full_sync(&mock).await;

    let quiet = MockImap::start(legacy_config(6)).await;
    let report = fx.delta(&quiet).await;

    assert!(
        !report.unchanged,
        "a modseq-less server can never claim this"
    );
    assert_eq!(report.new_messages, 0);
    assert_eq!(report.flag_updates, 0);
    assert_eq!(report.expunged, 0);
    assert!(
        body_fetches(&quiet).is_empty(),
        "nothing was re-downloaded: {:?}",
        quiet.commands()
    );
}

#[tokio::test]
async fn a_condstore_server_falls_back_until_it_has_a_baseline() {
    // The initial walk does not record a modseq — it has no way to know one is
    // meaningful for a folder it has only partly downloaded. So the first delta
    // after it enumerates, and *that* run establishes the baseline the cheap
    // strategy needs.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    fx.full_sync(&mock).await;
    assert_eq!(
        fx.sync_state().highestmodseq,
        None,
        "the walk leaves no modseq behind"
    );

    let first = fx.delta(&mock).await;
    assert_eq!(first.strategy, DeltaStrategy::UidDiff);
    assert_eq!(
        first.highestmodseq,
        Some(1),
        "but the fallback records one on the way out"
    );

    let second = fx.delta(&mock).await;
    assert_eq!(
        second.strategy,
        DeltaStrategy::Qresync,
        "so the next run gets the cheap path"
    );
    assert!(second.unchanged);
}

#[tokio::test]
async fn turning_qresync_off_forces_the_diff_on_a_capable_server() {
    // `sync.qresync = false` exists because some servers advertise CONDSTORE
    // and then report modseqs that go backwards. Switching it off has to
    // actually change what the engine sends, not just what it logs.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;
    assert_eq!(
        fx.delta(&mock).await.strategy,
        DeltaStrategy::Qresync,
        "this server is fully capable"
    );

    let capable = crate::imap::ImapCapabilities {
        idle: true,
        condstore: true,
        qresync: true,
        move_: true,
    };
    let off = MockImap::start(mock_config(3).change(2, &["\\Answered"], 5)).await;
    let report = fx.delta_claiming(&off, capable.without_modseq()).await;

    assert_eq!(report.strategy, DeltaStrategy::UidDiff);
    assert_eq!(
        report.flag_updates, 1,
        "and the diff still catches the change"
    );
    assert!(
        !off.commands()
            .iter()
            .any(|c| c.to_ascii_uppercase().contains("CHANGEDSINCE")),
        "no modseq question was asked: {:?}",
        off.commands()
    );
}

#[tokio::test]
async fn the_uidvalidity_check_still_applies_without_a_modseq() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(legacy_config(3)).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;
    assert_eq!(fx.message_count(), 3);

    let rekeyed = MockImap::start(legacy_config(3).uidvalidity(9)).await;
    let report = fx.delta(&rekeyed).await;

    assert_eq!(report.strategy, DeltaStrategy::Full);
    assert!(report.resynced);
    assert_eq!(report.purged_stale, 3);
    assert_eq!(fx.uids_at(9), vec![1, 2, 3]);
    assert_eq!(
        fx.message_count(),
        3,
        "the folder is replaced, not duplicated"
    );
    assert_eq!(
        fx.sync_state().highestmodseq,
        None,
        "a modseq-less server leaves the checkpoint empty, so the next run \
         enumerates again"
    );
}

#[tokio::test]
async fn a_search_that_returns_nothing_does_not_empty_the_folder() {
    // The enumeration is trusted to *delete*. A server having a bad day that
    // answers UID SEARCH with nothing would, taken at face value, wipe every
    // message and every thread in the mailbox — irreversibly, and on a folder
    // the same SELECT just said was full. SELECT's EXISTS count is the free
    // cross-check that turns a catastrophe into a retry.
    let fx = Fixture::open().await;
    let mock = MockImap::start(legacy_config(4)).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;
    assert_eq!(fx.message_count(), 4);

    let broken = MockImap::start(legacy_config(4).with_broken_search()).await;
    let report = fx.delta(&broken).await;

    assert_eq!(report.expunged, 0, "no deletion on an implausible answer");
    assert_eq!(fx.stored_uids(), vec![1, 2, 3, 4]);
    assert_eq!(fx.thread_count(), 4);
    assert!(
        !report.cancelled,
        "the run finished; it simply declined to act on the search"
    );

    // And the folder recovers by itself once the server does.
    let recovered = MockImap::start(legacy_with(&[1, 4])).await;
    assert_eq!(fx.delta(&recovered).await.expunged, 2);
    assert_eq!(fx.stored_uids(), vec![1, 4]);
}

#[tokio::test]
async fn a_broken_search_does_not_advance_the_checkpoint() {
    // Same guard, seen from the checkpoint side: a run that refused to trust
    // the server must not record a mark implying it did.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(3)).await;
    fx.full_sync(&mock).await;
    fx.delta(&mock).await;
    let before = fx.sync_state();
    assert_eq!(before.highestmodseq, Some(1));

    // A CONDSTORE server with no stored baseline yet, so the run enumerates —
    // and the enumeration comes back empty.
    let broken = MockImap::start(
        mock_config(3)
            .change(2, &["\\Flagged"], 12)
            .with_broken_search(),
    )
    .await;
    let capable = crate::imap::ImapCapabilities {
        idle: true,
        condstore: true,
        qresync: true,
        move_: true,
    };
    let report = fx.delta_claiming(&broken, capable.without_modseq()).await;

    assert_eq!(report.strategy, DeltaStrategy::UidDiff);
    assert_eq!(report.expunged, 0);
    assert_eq!(
        fx.sync_state().highestmodseq,
        before.highestmodseq,
        "the checkpoint held, so the next run asks again"
    );
    assert_eq!(fx.message_count(), 3);
}
