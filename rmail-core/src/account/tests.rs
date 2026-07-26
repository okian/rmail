//! Account CRUD tests over a real (temp-file) WAL database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::credential::CredentialSource;
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
        let path = std::env::temp_dir().join(format!("rmail-account-{pid}-{n}.db"));
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
async fn create_and_get_roundtrip_with_credential() {
    let tmp = TempDb::open();
    let created = create(
        &tmp.db,
        NewAccount {
            name: "Personal".to_owned(),
            imap_server: Some("imap.fastmail.com".to_owned()),
            imap_port: Some(993),
            username: Some("me@example.com".to_owned()),
            credential: CredentialSource::Env("FASTMAIL_PW".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(created.name, "Personal");
    assert_eq!(
        created.credential,
        CredentialSource::Env("FASTMAIL_PW".to_owned())
    );

    let fetched = get(&tmp.db, created.id).await.unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.imap_port, Some(993));
    // The credential source round-trips through storage.
    assert_eq!(
        fetched.credential,
        CredentialSource::Env("FASTMAIL_PW".to_owned())
    );
}

#[tokio::test]
async fn empty_name_is_invalid_argument() {
    let tmp = TempDb::open();
    let err = create(
        &tmp.db,
        NewAccount {
            name: "   ".to_owned(),
            ..Default::default()
        },
    )
    .await
    .expect_err("empty name must be rejected");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn duplicate_name_is_already_exists() {
    let tmp = TempDb::open();
    create(
        &tmp.db,
        NewAccount {
            name: "Work".to_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let err = create(
        &tmp.db,
        NewAccount {
            name: "Work".to_owned(),
            ..Default::default()
        },
    )
    .await
    .expect_err("duplicate name must be rejected");
    assert_eq!(err.reason(), ErrorReason::AlreadyExists);
}

#[tokio::test]
async fn get_missing_is_not_found() {
    let tmp = TempDb::open();
    let err = get(&tmp.db, 999).await.expect_err("missing account");
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn list_returns_all_ordered() {
    let tmp = TempDb::open();
    for name in ["Work", "Personal"] {
        create(
            &tmp.db,
            NewAccount {
                name: name.to_owned(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let all = list(&tmp.db).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].name, "Personal"); // ordered by name
    assert_eq!(all[1].name, "Work");
    // Accounts created without a credential default to None.
    assert_eq!(all[0].credential, CredentialSource::None);
}

#[tokio::test]
async fn delete_removes_and_missing_is_not_found() {
    let tmp = TempDb::open();
    let account = create(
        &tmp.db,
        NewAccount {
            name: "Temp".to_owned(),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    delete(&tmp.db, account.id).await.unwrap();
    assert_eq!(
        get(&tmp.db, account.id).await.expect_err("gone").reason(),
        ErrorReason::NotFound
    );

    // Deleting again reports NotFound rather than silently succeeding.
    assert_eq!(
        delete(&tmp.db, account.id)
            .await
            .expect_err("already gone")
            .reason(),
        ErrorReason::NotFound
    );
}

#[tokio::test]
async fn keychain_without_username_is_invalid_argument() {
    let tmp = TempDb::open();
    let err = create(
        &tmp.db,
        NewAccount {
            name: "NoUser".to_owned(),
            username: None,
            credential: CredentialSource::Keychain("fastmail".to_owned()),
            ..Default::default()
        },
    )
    .await
    .expect_err("keychain without username must be rejected");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);

    // With a username it persists and round-trips.
    let ok = create(
        &tmp.db,
        NewAccount {
            name: "WithUser".to_owned(),
            username: Some("me@example.com".to_owned()),
            credential: CredentialSource::Keychain("fastmail".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        ok.credential,
        CredentialSource::Keychain("fastmail".to_owned())
    );
}

#[tokio::test]
async fn command_credential_source_persists() {
    let tmp = TempDb::open();
    let created = create(
        &tmp.db,
        NewAccount {
            name: "Cmd".to_owned(),
            credential: CredentialSource::Command("security find-generic-password -w".to_owned()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let fetched = get(&tmp.db, created.id).await.unwrap();
    assert_eq!(
        fetched.credential,
        CredentialSource::Command("security find-generic-password -w".to_owned())
    );
}
