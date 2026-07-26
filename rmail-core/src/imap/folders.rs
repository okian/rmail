//! IMAP folder discovery and persistence into the `mailboxes` table.

use async_imap::types::NameAttribute;
use async_imap::Session;
use futures::StreamExt;

use super::conn::ImapStream;
use super::{map_imap_err, FolderInfo};
use crate::error::Error;
use crate::repo;
use crate::storage::Database;

/// List all folders via `LIST "" "*"`.
///
/// # Errors
///
/// [`Error::Unavailable`] if the `LIST` command or its stream fails.
pub async fn list_folders<T: ImapStream>(
    session: &mut Session<T>,
) -> Result<Vec<FolderInfo>, Error> {
    let mut stream = session.list(None, Some("*")).await.map_err(map_imap_err)?;

    let mut folders = Vec::new();
    while let Some(item) = stream.next().await {
        let name = item.map_err(map_imap_err)?;
        let selectable = !name
            .attributes()
            .iter()
            .any(|attr| matches!(attr, NameAttribute::NoSelect));
        folders.push(FolderInfo {
            name: name.name().to_owned(),
            delimiter: name.delimiter().map(str::to_owned),
            selectable,
        });
    }
    Ok(folders)
}

/// Persist discovered folders into `mailboxes` for `account_id` (idempotent
/// upsert by `(account_id, name)`), in a single transaction.
///
/// # Errors
///
/// A mapped storage error.
pub async fn store_folders(
    db: &Database,
    account_id: i64,
    folders: &[FolderInfo],
) -> Result<(), Error> {
    let folders = folders.to_vec();
    db.write(move |conn| {
        let tx = conn.transaction()?;
        for folder in &folders {
            // Record the one operationally-critical attribute (selectability);
            // richer attribute capture can follow when sync needs it.
            let attributes = if folder.selectable {
                None
            } else {
                Some("\\Noselect")
            };
            repo::upsert_mailbox(&tx, account_id, &folder.name, attributes)?;
        }
        tx.commit()?;
        Ok(())
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::imap::conn::login;
    use crate::imap::mock::{MockConfig, MockImap};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDb {
        db: Database,
        path: PathBuf,
    }

    impl TempDb {
        fn open() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("rmail-imapfolders-{pid}-{n}.db"));
            let db = Database::open(&path).unwrap();
            Self { db, path }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ =
                    std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
            }
        }
    }

    fn mock_with_folders() -> MockConfig {
        MockConfig::default().password("pw").folders(vec![
            ("INBOX", ""),
            ("Archive", "\\HasNoChildren"),
            ("[Gmail]", "\\Noselect \\HasChildren"),
        ])
    }

    #[tokio::test]
    async fn list_parses_folders_and_selectability() {
        let mock = MockImap::start(mock_with_folders()).await;
        let stream = tokio::net::TcpStream::connect(mock.addr).await.unwrap();
        let mut session = login(stream, "user", "pw").await.unwrap();

        let folders = list_folders(&mut session).await.unwrap();
        let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["INBOX", "Archive", "[Gmail]"]);

        let gmail = folders.iter().find(|f| f.name == "[Gmail]").unwrap();
        assert!(!gmail.selectable, "\\Noselect folder is not selectable");
        assert_eq!(gmail.delimiter.as_deref(), Some("/"));

        let inbox = folders.iter().find(|f| f.name == "INBOX").unwrap();
        assert!(inbox.selectable);

        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn store_folders_populates_mailboxes_idempotently() {
        let tmp = TempDb::open();
        let account_id = tmp
            .db
            .write(|c| {
                repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();

        let folders = vec![
            FolderInfo {
                name: "INBOX".to_owned(),
                delimiter: Some("/".to_owned()),
                selectable: true,
            },
            FolderInfo {
                name: "[Gmail]".to_owned(),
                delimiter: Some("/".to_owned()),
                selectable: false,
            },
        ];

        store_folders(&tmp.db, account_id, &folders).await.unwrap();
        // Re-running discovery is idempotent (upsert by (account, name)).
        store_folders(&tmp.db, account_id, &folders).await.unwrap();

        let stored = tmp
            .db
            .read(move |c| repo::list_mailboxes(c, account_id))
            .await
            .unwrap();
        assert_eq!(stored.len(), 2, "no duplicate rows on re-discovery");
        let gmail = stored.iter().find(|m| m.name == "[Gmail]").unwrap();
        assert_eq!(gmail.attributes.as_deref(), Some("\\Noselect"));
    }

    #[tokio::test]
    async fn full_session_logs_in_lists_and_persists() {
        // Exercises the orchestrator end-to-end (login -> probe -> list -> store)
        // against the in-process mock, proving folder discovery populates
        // mailboxes.
        let tmp = TempDb::open();
        let account_id = tmp
            .db
            .write(|c| {
                repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();

        let mock = MockImap::start(mock_with_folders()).await;
        let stream = tokio::net::TcpStream::connect(mock.addr).await.unwrap();
        let report = crate::imap::run_session(&tmp.db, account_id, "user", "pw", stream)
            .await
            .unwrap();

        assert_eq!(report.folders.len(), 3);
        assert!(report.capabilities.idle && report.capabilities.qresync);

        let stored = tmp
            .db
            .read(move |c| repo::list_mailboxes(c, account_id))
            .await
            .unwrap();
        assert_eq!(stored.len(), 3, "discovery populated mailboxes");
    }
}
