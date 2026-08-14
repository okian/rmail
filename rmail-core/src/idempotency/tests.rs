//! Fence behaviour over a real (temp-file) WAL database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDb {
    db: Database,
    path: PathBuf,
}

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-idem-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        Self { db, path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn store(tmp: &TempDb) -> IdempotencyStore {
    IdempotencyStore::new(
        tmp.db.clone(),
        Duration::from_secs(3600),
        Duration::from_secs(300),
    )
}

const METHOD: &str = "/rmail.v1.MailService/Move";

#[tokio::test]
async fn a_first_claim_is_fresh() {
    let tmp = TempDb::open();
    let store = store(&tmp);
    assert_eq!(
        store.claim("k1", METHOD, b"payload").await.unwrap(),
        Claim::Fresh
    );
}

#[tokio::test]
async fn a_recorded_response_is_replayed_not_re_applied() {
    let tmp = TempDb::open();
    let store = store(&tmp);
    assert_eq!(
        store.claim("k1", METHOD, b"payload").await.unwrap(),
        Claim::Fresh
    );
    store.record("k1", b"response".to_vec()).await.unwrap();

    assert_eq!(
        store.claim("k1", METHOD, b"payload").await.unwrap(),
        Claim::Replay(b"response".to_vec())
    );
}

#[tokio::test]
async fn an_empty_response_still_replays() {
    // Every MailService mutation returns google.protobuf.Empty, which encodes
    // to zero bytes. If "no bytes" were read back as "no outcome", the one
    // group of RPCs this fence exists for would never replay at all.
    let tmp = TempDb::open();
    let store = store(&tmp);
    store.claim("k1", METHOD, b"payload").await.unwrap();
    store.record("k1", Vec::new()).await.unwrap();

    assert_eq!(
        store.claim("k1", METHOD, b"payload").await.unwrap(),
        Claim::Replay(Vec::new())
    );
}

#[tokio::test]
async fn a_differing_payload_under_the_same_key_is_already_exists() {
    let tmp = TempDb::open();
    let store = store(&tmp);
    store.claim("k1", METHOD, b"move to Archive").await.unwrap();
    store.record("k1", Vec::new()).await.unwrap();

    let err = store
        .claim("k1", METHOD, b"move to Trash")
        .await
        .expect_err("a changed payload must not replay");
    assert_eq!(err.reason(), ErrorReason::AlreadyExists);
}

#[tokio::test]
async fn a_key_reused_across_methods_is_already_exists() {
    // The method is folded into the hash, so a key reused on a different RPC
    // conflicts rather than replaying the wrong method's response.
    let tmp = TempDb::open();
    let store = store(&tmp);
    store.claim("k1", METHOD, b"payload").await.unwrap();
    store
        .record("k1", b"an OutboxEntry".to_vec())
        .await
        .unwrap();

    let err = store
        .claim("k1", "/rmail.v1.MailService/Delete", b"payload")
        .await
        .expect_err("cross-method reuse must be refused");
    assert_eq!(err.reason(), ErrorReason::AlreadyExists);
}

#[tokio::test]
async fn an_unfinished_claim_is_aborted_not_re_applied() {
    // The crash case: the fence is committed, the process dies, the client
    // retries. Re-applying is the one thing that must not happen.
    let tmp = TempDb::open();
    let store = store(&tmp);
    store.claim("k1", METHOD, b"payload").await.unwrap();

    let err = store
        .claim("k1", METHOD, b"payload")
        .await
        .expect_err("an unfinished claim must not be handed out twice");
    assert_eq!(err.reason(), ErrorReason::Aborted);
}

#[tokio::test]
async fn a_released_claim_can_be_retried() {
    let tmp = TempDb::open();
    let store = store(&tmp);
    store.claim("k1", METHOD, b"payload").await.unwrap();
    store.release("k1").await.unwrap();

    assert_eq!(
        store.claim("k1", METHOD, b"payload").await.unwrap(),
        Claim::Fresh
    );
}

#[tokio::test]
async fn releasing_never_drops_a_recorded_response() {
    // `release` is called on the failure path. If it could delete a completed
    // row, a handler that failed *after* recording would erase the replay the
    // client is about to ask for.
    let tmp = TempDb::open();
    let store = store(&tmp);
    store.claim("k1", METHOD, b"payload").await.unwrap();
    store.record("k1", b"response".to_vec()).await.unwrap();
    store.release("k1").await.unwrap();

    assert_eq!(
        store.claim("k1", METHOD, b"payload").await.unwrap(),
        Claim::Replay(b"response".to_vec())
    );
}

/// A store whose windows are the shortest the floor allows, so a lapse is
/// observable in ~a second rather than a day.
fn short(tmp: &TempDb) -> IdempotencyStore {
    IdempotencyStore::new(
        tmp.db.clone(),
        Duration::from_secs(0),
        Duration::from_secs(0),
    )
}

/// Past `expires_at` for a store built by [`short`]. `expires_at` is
/// second-granular (`unixepoch()`), so this crosses exactly one tick.
async fn wait_for_the_fence_to_lapse() {
    tokio::time::sleep(Duration::from_millis(1_100)).await;
}

#[tokio::test]
async fn a_lapsed_claim_is_reclaimable() {
    let tmp = TempDb::open();
    let store = short(&tmp);
    store.claim("k1", METHOD, b"payload").await.unwrap();
    store.record("k1", b"response".to_vec()).await.unwrap();
    wait_for_the_fence_to_lapse().await;

    assert_eq!(
        store.claim("k1", METHOD, b"payload").await.unwrap(),
        Claim::Fresh,
        "an expired fence must not keep replaying"
    );
}

#[tokio::test]
async fn an_unfinished_claim_lapses_on_the_short_window_not_the_long_one() {
    // The common cause of an unfinished claim is a client whose deadline
    // elapsed — tonic drops the handler before it can record or release, and
    // that client's very next act is to retry the same key. Fencing it for the
    // full retention would break the workflow the key exists for.
    let tmp = TempDb::open();
    let store = IdempotencyStore::new(
        tmp.db.clone(),
        Duration::from_secs(86_400),
        Duration::from_secs(0),
    );
    store.claim("k1", METHOD, b"payload").await.unwrap();
    assert_eq!(
        store
            .claim("k1", METHOD, b"payload")
            .await
            .expect_err("still inside the in-flight window")
            .reason(),
        ErrorReason::Aborted
    );

    wait_for_the_fence_to_lapse().await;
    assert_eq!(
        store.claim("k1", METHOD, b"payload").await.unwrap(),
        Claim::Fresh,
        "an abandoned claim must not stay fenced for the whole retention"
    );
}

#[tokio::test]
async fn recording_extends_the_fence_past_the_in_flight_window() {
    // The mirror of the test above: once there *is* an outcome, the short
    // window is the wrong one — a replay has to outlive it.
    let tmp = TempDb::open();
    let store = IdempotencyStore::new(
        tmp.db.clone(),
        Duration::from_secs(86_400),
        Duration::from_secs(0),
    );
    store.claim("k1", METHOD, b"payload").await.unwrap();
    store.record("k1", b"response".to_vec()).await.unwrap();
    wait_for_the_fence_to_lapse().await;

    assert_eq!(
        store.claim("k1", METHOD, b"payload").await.unwrap(),
        Claim::Replay(b"response".to_vec()),
        "a recorded response must survive the in-flight window"
    );
}

#[tokio::test]
async fn purge_removes_only_lapsed_claims() {
    let tmp = TempDb::open();
    let live = store(&tmp);
    live.claim("live", METHOD, b"payload").await.unwrap();
    let lapsed = short(&tmp);
    lapsed.claim("lapsed", METHOD, b"payload").await.unwrap();
    wait_for_the_fence_to_lapse().await;

    assert_eq!(lapsed.purge_expired().await.unwrap(), 1);
    // The live claim survived: reclaiming it is still refused.
    assert_eq!(
        live.claim("live", METHOD, b"payload")
            .await
            .expect_err("live claim should still be fenced")
            .reason(),
        ErrorReason::Aborted
    );
}

#[tokio::test]
async fn a_bad_key_is_invalid_argument() {
    let tmp = TempDb::open();
    let store = store(&tmp);
    for key in [
        String::new(),
        "x".repeat(MAX_KEY_LEN + 1),
        "has\nnewline".to_owned(),
        "has\ttab".to_owned(),
    ] {
        let err = store
            .claim(&key, METHOD, b"payload")
            .await
            .expect_err("bad key must be refused");
        assert_eq!(err.reason(), ErrorReason::InvalidArgument, "for {key:?}");
    }
    // A UUID is the documented shape and must be accepted.
    assert_eq!(
        store
            .claim("6f1c9a0e-2b3d-4c5f-8a91-0b2c3d4e5f60", METHOD, b"payload")
            .await
            .unwrap(),
        Claim::Fresh
    );
}

#[tokio::test]
async fn recording_an_unknown_key_is_not_an_error() {
    // The claim lapsed while the handler ran. The mutation succeeded; failing
    // the call now would make the caller retry something already applied.
    let tmp = TempDb::open();
    let store = store(&tmp);
    store
        .record("never-claimed", b"response".to_vec())
        .await
        .unwrap();
}

#[tokio::test]
async fn two_concurrent_claims_never_both_win() {
    // The single-writer transaction is what makes this true; without it both
    // callers would read "absent" and both would insert.
    let tmp = TempDb::open();
    let store = store(&tmp);
    let a = store.clone();
    let b = store.clone();
    let (left, right) = tokio::join!(
        async move { a.claim("race", METHOD, b"payload").await },
        async move { b.claim("race", METHOD, b"payload").await },
    );

    let fresh = [&left, &right]
        .iter()
        .filter(|r| matches!(r, Ok(Claim::Fresh)))
        .count();
    assert_eq!(fresh, 1, "exactly one caller may own the mutation");
    let aborted = [&left, &right]
        .iter()
        .filter(|r| matches!(r, Err(e) if e.reason() == ErrorReason::Aborted))
        .count();
    assert_eq!(aborted, 1, "the loser must be told, not silently allowed");
}
