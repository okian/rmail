//! Token mint/list/revoke/verify tests over a real (temp-file) WAL database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::storage::Database;
use crate::ErrorReason;

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
        let path = std::env::temp_dir().join(format!("rmail-auth-{pid}-{n}.db"));
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

fn new_token(name: &str, scopes: Vec<Scope>) -> NewToken {
    NewToken {
        name: name.to_owned(),
        scopes,
        ttl_secs: None,
    }
}

#[tokio::test]
async fn mint_then_verify_round_trips_the_granted_scopes() {
    let tmp = TempDb::open();
    let minted = mint(
        &tmp.db,
        new_token("ci", vec![Scope::MailRead, Scope::AiInvoke]),
    )
    .await
    .unwrap();

    assert_eq!(minted.token.name, "ci");
    assert!(minted.secret.starts_with("rmail_tok_"));
    assert!(!minted.token.revoked);
    assert!(minted.token.expires_at.is_none());

    let verified = verify(&tmp.db, &minted.secret).await.unwrap();
    assert_eq!(verified.id, minted.token.id);
    assert_eq!(verified.scopes, vec![Scope::MailRead, Scope::AiInvoke]);
}

#[tokio::test]
async fn verify_records_last_used_at() {
    let tmp = TempDb::open();
    let minted = mint(&tmp.db, new_token("ci", vec![Scope::MailRead]))
        .await
        .unwrap();
    verify(&tmp.db, &minted.secret).await.unwrap();

    let listed = list(&tmp.db).await.unwrap();
    let row = listed.iter().find(|t| t.id == minted.token.id).unwrap();
    assert!(row.last_used_at.is_some());
}

#[tokio::test]
async fn a_wrong_secret_is_rejected() {
    let tmp = TempDb::open();
    let minted = mint(&tmp.db, new_token("ci", vec![Scope::MailRead]))
        .await
        .unwrap();
    // Same id, different (well-formed) secret.
    let forged = format!("rmail_tok_{}_{}", minted.token.id, "0".repeat(64));
    assert_ne!(forged, minted.secret);

    let err = verify(&tmp.db, &forged).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);
}

#[tokio::test]
async fn an_unknown_token_id_is_rejected() {
    let tmp = TempDb::open();
    let phantom = format!("rmail_tok_999999_{}", "a".repeat(64));
    let err = verify(&tmp.db, &phantom).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);
}

#[tokio::test]
async fn malformed_bearer_strings_are_rejected() {
    let tmp = TempDb::open();
    for bad in [
        "",
        "not-a-token",
        "rmail_tok_",
        "rmail_tok_abc_secret", // non-numeric id
        "rmail_tok_1_",         // empty secret
        "rmail_tok_1",          // no secret segment
    ] {
        let err = verify(&tmp.db, bad).await.unwrap_err();
        assert_eq!(err.reason(), ErrorReason::Unauthenticated, "input {bad:?}");
    }
}

#[tokio::test]
async fn a_revoked_token_is_rejected() {
    let tmp = TempDb::open();
    let minted = mint(&tmp.db, new_token("ci", vec![Scope::MailRead]))
        .await
        .unwrap();

    // Valid before revocation.
    verify(&tmp.db, &minted.secret).await.unwrap();

    revoke(&tmp.db, minted.token.id).await.unwrap();

    let err = verify(&tmp.db, &minted.secret).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);

    // Reflected in ListTokens' view too.
    let listed = list(&tmp.db).await.unwrap();
    let row = listed.iter().find(|t| t.id == minted.token.id).unwrap();
    assert!(row.revoked);
}

#[tokio::test]
async fn revoking_an_already_revoked_token_is_not_an_error() {
    let tmp = TempDb::open();
    let minted = mint(&tmp.db, new_token("ci", vec![Scope::MailRead]))
        .await
        .unwrap();
    revoke(&tmp.db, minted.token.id).await.unwrap();
    revoke(&tmp.db, minted.token.id).await.unwrap(); // idempotent
}

#[tokio::test]
async fn revoking_an_id_that_never_existed_is_not_found() {
    let tmp = TempDb::open();
    let err = revoke(&tmp.db, 424_242).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn an_expired_token_is_rejected() {
    let tmp = TempDb::open();
    let minted = mint(
        &tmp.db,
        NewToken {
            name: "short-lived".to_owned(),
            scopes: vec![Scope::MailRead],
            // A long TTL, not a short one: the "still valid" assertion below
            // must not race the argon2 hashing this test itself performs, and
            // the actual expiry is forced directly via SQL regardless.
            ttl_secs: Some(3600),
        },
    )
    .await
    .unwrap();
    assert!(minted.token.expires_at.is_some());

    // Still valid immediately after minting.
    verify(&tmp.db, &minted.secret).await.unwrap();

    // Force it into the past directly rather than sleeping in a test.
    tmp.db
        .write(move |c| {
            c.execute(
                "UPDATE api_tokens SET expires_at = 1 WHERE id = ?1",
                [minted.token.id],
            )
        })
        .await
        .unwrap();

    let err = verify(&tmp.db, &minted.secret).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);
}

#[tokio::test]
async fn mint_rejects_an_empty_name() {
    let tmp = TempDb::open();
    let err = mint(&tmp.db, new_token("   ", vec![Scope::MailRead]))
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn mint_rejects_an_empty_scope_list() {
    let tmp = TempDb::open();
    let err = mint(&tmp.db, new_token("ci", vec![])).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn mint_rejects_a_non_positive_ttl() {
    let tmp = TempDb::open();
    let err = mint(
        &tmp.db,
        NewToken {
            name: "ci".to_owned(),
            scopes: vec![Scope::MailRead],
            ttl_secs: Some(0),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn dummy_hash_is_a_genuinely_parseable_phc_string() {
    // Guards the point of `DUMMY_HASH`: if it failed to parse, `verify_secret`
    // would reject it instantly instead of paying the argon2 cost, silently
    // reopening the exact timing oracle the constant exists to close.
    use argon2::password_hash::PasswordHash;
    PasswordHash::new(DUMMY_HASH).expect("DUMMY_HASH must be a valid PHC string");
}

#[tokio::test]
async fn an_unknown_id_pays_the_same_argon2_cost_as_a_real_verify() {
    // A weak proxy for "no timing oracle": both paths must actually invoke
    // the expensive comparison, which `dummy_hash_is_a_genuinely_parseable_phc_string`
    // establishes is possible; here, a wrong secret against DUMMY_HASH must
    // fail the same way a wrong secret against a real hash does.
    let tmp = TempDb::open();
    let minted = mint(&tmp.db, new_token("ci", vec![Scope::MailRead]))
        .await
        .unwrap();

    let wrong_secret_real_id = format!("rmail_tok_{}_{}", minted.token.id, "0".repeat(64));
    let unknown_id = format!("rmail_tok_999999_{}", "0".repeat(64));

    let err_a = verify(&tmp.db, &wrong_secret_real_id).await.unwrap_err();
    let err_b = verify(&tmp.db, &unknown_id).await.unwrap_err();
    assert_eq!(err_a.reason(), ErrorReason::Unauthenticated);
    assert_eq!(err_b.reason(), ErrorReason::Unauthenticated);
}

#[tokio::test]
async fn list_orders_newest_first_and_two_secrets_never_collide() {
    let tmp = TempDb::open();
    let a = mint(&tmp.db, new_token("a", vec![Scope::MailRead]))
        .await
        .unwrap();
    let b = mint(&tmp.db, new_token("b", vec![Scope::MailRead]))
        .await
        .unwrap();
    assert_ne!(a.secret, b.secret);

    let listed = list(&tmp.db).await.unwrap();
    assert_eq!(listed[0].id, b.token.id);
    assert_eq!(listed[1].id, a.token.id);
}
