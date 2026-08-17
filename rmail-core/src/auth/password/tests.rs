//! `client_password` set/clear/verify/lockout tests over a real (temp-file)
//! WAL database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::storage::Database;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDb {
    db: Database,
    path: PathBuf,
}

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-auth-password-{pid}-{n}.db"));
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

#[tokio::test]
async fn no_password_is_reported_as_not_configured_and_verify_says_the_same() {
    let tmp = TempDb::open();
    assert!(!is_configured(&tmp.db).await.unwrap());
    let outcome = verify_password(&tmp.db, "anything", 5, 900).await.unwrap();
    assert_eq!(outcome, LoginOutcome::NotConfigured);
}

#[tokio::test]
async fn set_then_verify_the_right_password_succeeds() {
    let tmp = TempDb::open();
    set_password(&tmp.db, "correct horse battery staple")
        .await
        .unwrap();
    assert!(is_configured(&tmp.db).await.unwrap());

    let outcome = verify_password(&tmp.db, "correct horse battery staple", 5, 900)
        .await
        .unwrap();
    assert_eq!(outcome, LoginOutcome::Success);
}

#[tokio::test]
async fn the_wrong_password_is_rejected_and_counts_down_remaining_attempts() {
    let tmp = TempDb::open();
    set_password(&tmp.db, "hunter2").await.unwrap();

    let outcome = verify_password(&tmp.db, "wrong", 5, 900).await.unwrap();
    assert_eq!(outcome, LoginOutcome::WrongPassword { remaining: 4 });

    let outcome = verify_password(&tmp.db, "still wrong", 5, 900)
        .await
        .unwrap();
    assert_eq!(outcome, LoginOutcome::WrongPassword { remaining: 3 });
}

#[tokio::test]
async fn an_empty_password_is_refused_rather_than_stored() {
    let tmp = TempDb::open();
    let err = set_password(&tmp.db, "").await.unwrap_err();
    assert!(matches!(err, crate::Error::InvalidArgument(_)));
    assert!(!is_configured(&tmp.db).await.unwrap());
}

#[tokio::test]
async fn reaching_max_attempts_locks_out_even_the_correct_password() {
    let tmp = TempDb::open();
    set_password(&tmp.db, "hunter2").await.unwrap();

    // max_attempts = 2: the second wrong guess trips the lockout.
    let first = verify_password(&tmp.db, "wrong", 2, 900).await.unwrap();
    assert_eq!(first, LoginOutcome::WrongPassword { remaining: 1 });

    let second = verify_password(&tmp.db, "wrong", 2, 900).await.unwrap();
    assert!(matches!(second, LoginOutcome::LockedOut { .. }));

    // The *correct* password is refused too, while locked out — the point of
    // a lockout is that it stops checking, not that it stops checking wrong
    // guesses specifically.
    let third = verify_password(&tmp.db, "hunter2", 2, 900).await.unwrap();
    assert!(matches!(third, LoginOutcome::LockedOut { .. }));
}

#[tokio::test]
async fn a_lockout_that_has_expired_lets_verification_run_again() {
    let tmp = TempDb::open();
    set_password(&tmp.db, "hunter2").await.unwrap();

    // lockout_secs = 0: the deadline is "now", which has already passed by
    // the time the next call reads it back.
    let tripped = verify_password(&tmp.db, "wrong", 1, 0).await.unwrap();
    assert!(matches!(tripped, LoginOutcome::LockedOut { .. }));

    let outcome = verify_password(&tmp.db, "hunter2", 1, 0).await.unwrap();
    assert_eq!(outcome, LoginOutcome::Success);
}

#[tokio::test]
async fn a_successful_login_resets_the_failure_count() {
    let tmp = TempDb::open();
    set_password(&tmp.db, "hunter2").await.unwrap();

    verify_password(&tmp.db, "wrong", 5, 900).await.unwrap();
    verify_password(&tmp.db, "wrong", 5, 900).await.unwrap();
    let ok = verify_password(&tmp.db, "hunter2", 5, 900).await.unwrap();
    assert_eq!(ok, LoginOutcome::Success);

    // Back to full budget, not continuing from the earlier failures.
    let outcome = verify_password(&tmp.db, "wrong again", 5, 900)
        .await
        .unwrap();
    assert_eq!(outcome, LoginOutcome::WrongPassword { remaining: 4 });
}

#[tokio::test]
async fn setting_a_new_password_clears_an_active_lockout() {
    let tmp = TempDb::open();
    set_password(&tmp.db, "old-password").await.unwrap();
    let tripped = verify_password(&tmp.db, "wrong", 1, 900).await.unwrap();
    assert!(matches!(tripped, LoginOutcome::LockedOut { .. }));

    set_password(&tmp.db, "new-password").await.unwrap();

    // A higher max_attempts than the trip above, so the two outcomes are
    // distinguishable: if the old lockout (or its failure count) survived,
    // this first guess would come back locked out instead of an ordinary
    // first failure with a full budget remaining.
    let outcome = verify_password(&tmp.db, "old-password", 2, 900)
        .await
        .unwrap();
    assert_eq!(outcome, LoginOutcome::WrongPassword { remaining: 1 });

    let outcome = verify_password(&tmp.db, "new-password", 2, 900)
        .await
        .unwrap();
    assert_eq!(outcome, LoginOutcome::Success);
}

#[tokio::test]
async fn clearing_the_password_removes_the_gate_entirely() {
    let tmp = TempDb::open();
    set_password(&tmp.db, "hunter2").await.unwrap();
    assert!(is_configured(&tmp.db).await.unwrap());

    clear_password(&tmp.db).await.unwrap();
    assert!(!is_configured(&tmp.db).await.unwrap());

    let outcome = verify_password(&tmp.db, "hunter2", 5, 900).await.unwrap();
    assert_eq!(outcome, LoginOutcome::NotConfigured);
}

#[tokio::test]
async fn clearing_an_already_absent_password_is_not_an_error() {
    let tmp = TempDb::open();
    clear_password(&tmp.db).await.unwrap();
    clear_password(&tmp.db).await.unwrap();
}
