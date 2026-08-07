//! What task 60 owes at the storage layer: drafts survive a round trip
//! through SQLite unchanged (including recipient *order*, display names, and
//! binary attachment bytes), threading headers are resolved from the parent
//! once and stay put afterwards, and every documented error path returns the
//! reason the gRPC boundary will map to a status code.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};

use super::*;
use crate::message::parse_message;
use crate::repo;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: AtomicI64,
    path: PathBuf,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-compose-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
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
            account_id,
            mailbox_id,
            next_uid: AtomicI64::new(1),
            path,
        }
    }

    fn store(&self) -> DraftStore {
        DraftStore::new(self.db.clone())
    }

    /// Insert a message with the given threading headers, returning its id.
    fn message(
        &self,
        message_id: Option<&str>,
        in_reply_to: Option<&str>,
        references: Option<&str>,
    ) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let (message_id, in_reply_to, references) = (
            message_id.map(str::to_owned),
            in_reply_to.map(str::to_owned),
            references.map(str::to_owned),
        );
        self.db
            .with_write(move |c| {
                c.execute(
                    "INSERT INTO messages
                         (account_id, mailbox_id, uid, uidvalidity, message_id, in_reply_to,
                          references_hdr, subject)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, 'Parent')",
                    rusqlite::params![
                        account_id,
                        mailbox_id,
                        uid,
                        message_id,
                        in_reply_to,
                        references
                    ],
                )?;
                Ok(c.last_insert_rowid())
            })
            .unwrap()
    }

    fn new_draft(&self) -> NewDraft {
        NewDraft {
            account_id: self.account_id,
            from: mailbox("Alice <alice@example.com>"),
            to: vec![mailbox("bob@example.net")],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Lunch".to_owned(),
            body_text: "Shall we say noon?".to_owned(),
            body_html: None,
            attachments: Vec::new(),
            in_reply_to_message_id: None,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn mailbox(spec: &str) -> Mailbox {
    Mailbox::parse(spec).unwrap()
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_draft_survives_a_round_trip_through_the_database() {
    let fx = Fixture::open();
    let store = fx.store();

    let created = store
        .create(NewDraft {
            from: mailbox("Café Ünicode <alice@example.com>"),
            to: vec![
                mailbox("\"Doe, Jane\" <jane@example.net>"),
                mailbox("bob@example.net"),
            ],
            cc: vec![mailbox("Carol <carol@example.org>")],
            bcc: vec![mailbox("secret@example.org")],
            subject: "Rapport für Q3".to_owned(),
            body_text: "Le rapport est joint.".to_owned(),
            body_html: Some("<p>joint</p>".to_owned()),
            attachments: vec![NewAttachment {
                filename: "réport.pdf".to_owned(),
                content_type: "application/pdf".to_owned(),
                content: (0u8..=255).cycle().take(1000).collect(),
            }],
            ..fx.new_draft()
        })
        .await
        .unwrap();

    let fetched = store.get(created.id).await.unwrap();
    assert_eq!(fetched, created);

    assert_eq!(fetched.from.display_name(), Some("Café Ünicode"));
    // Author order is preserved: recipients are a list, not a set.
    assert_eq!(
        fetched.to.iter().map(Mailbox::address).collect::<Vec<_>>(),
        vec!["jane@example.net", "bob@example.net"]
    );
    assert_eq!(fetched.to[0].display_name(), Some("Doe, Jane"));
    assert_eq!(fetched.cc[0].address(), "carol@example.org");
    assert_eq!(fetched.bcc[0].address(), "secret@example.org");
    assert_eq!(fetched.subject, "Rapport für Q3");
    assert_eq!(fetched.body_html.as_deref(), Some("<p>joint</p>"));
    assert_eq!(fetched.attachments.len(), 1);
    assert_eq!(fetched.attachments[0].filename, "réport.pdf");
    assert_eq!(fetched.attachments[0].size, 1000);
    assert_eq!(
        fetched.attachments[0].content,
        (0u8..=255).cycle().take(1000).collect::<Vec<u8>>()
    );
}

#[tokio::test]
async fn drafts_persist_across_reopening_the_database() {
    // "Drafts persist locally" is the acceptance bullet; proving it needs the
    // database handle to actually go away.
    let fx = Fixture::open();
    let id = fx.store().create(fx.new_draft()).await.unwrap().id;

    let reopened = Database::open(&fx.path).unwrap();
    let draft = DraftStore::new(reopened).get(id).await.unwrap();
    assert_eq!(draft.subject, "Lunch");
    assert_eq!(draft.to[0].address(), "bob@example.net");
}

#[tokio::test]
async fn list_is_newest_edited_first_and_omits_attachment_bytes() {
    let fx = Fixture::open();
    let store = fx.store();

    let first = store
        .create(NewDraft {
            subject: "first".to_owned(),
            attachments: vec![NewAttachment {
                filename: "a.bin".to_owned(),
                content_type: "application/octet-stream".to_owned(),
                content: vec![7u8; 4096],
            }],
            ..fx.new_draft()
        })
        .await
        .unwrap();
    let second = store
        .create(NewDraft {
            subject: "second".to_owned(),
            ..fx.new_draft()
        })
        .await
        .unwrap();

    // `unixepoch()` has one-second resolution, so both rows are created in the
    // same tick and nothing about the ORDER BY would be observable. Backdate
    // them to distinct instants instead of sleeping: what is under test is the
    // ordering and the fact that an edit advances `updated_at`, not the
    // timer's granularity.
    let (first_id, second_id) = (first.id, second.id);
    fx.db
        .with_write(move |c| {
            c.execute(
                "UPDATE drafts SET updated_at = 1000 WHERE id = ?1",
                [first_id],
            )?;
            c.execute(
                "UPDATE drafts SET updated_at = 2000 WHERE id = ?1",
                [second_id],
            )
        })
        .unwrap();

    let listed = store.list(fx.account_id, 0).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, second_id, "most recently edited first");
    assert_eq!(listed[1].id, first_id);

    // Editing the older one moves it to the front, because `update` stamps
    // `updated_at` with the current time — which is far past both backdates.
    let first = store
        .update(
            first.id,
            DraftPatch {
                subject: Some("first, edited".to_owned()),
                ..DraftPatch::default()
            },
        )
        .await
        .unwrap();
    assert!(first.updated_at > 2000, "an edit advances updated_at");

    let listed = store.list(fx.account_id, 0).await.unwrap();
    assert_eq!(listed[0].id, first_id, "an edit moves a draft to the front");
    assert_eq!(listed[1].id, second_id);

    let attachment = &listed[0].attachments[0];
    assert_eq!(attachment.size, 4096, "size is authoritative in a list");
    assert!(
        attachment.content.is_empty(),
        "a list must not carry attachment bytes"
    );
    assert_eq!(
        store.get(first.id).await.unwrap().attachments[0]
            .content
            .len(),
        4096,
        "but `get` does"
    );
}

#[tokio::test]
async fn list_is_scoped_to_one_account_and_capped() {
    let fx = Fixture::open();
    let store = fx.store();
    let other_account = fx
        .db
        .with_write(|c| {
            repo::insert_account(
                c,
                &repo::NewAccount {
                    name: "Work".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    for _ in 0..3 {
        store.create(fx.new_draft()).await.unwrap();
    }
    store
        .create(NewDraft {
            account_id: other_account,
            ..fx.new_draft()
        })
        .await
        .unwrap();

    assert_eq!(store.list(fx.account_id, 0).await.unwrap().len(), 3);
    assert_eq!(store.list(other_account, 0).await.unwrap().len(), 1);
    assert_eq!(store.list(fx.account_id, 2).await.unwrap().len(), 2);
    // Over the cap clamps rather than erroring.
    assert_eq!(
        store
            .list(fx.account_id, MAX_LIST_LIMIT + 1000)
            .await
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn an_update_replaces_only_the_fields_it_names() {
    let fx = Fixture::open();
    let store = fx.store();
    let created = store
        .create(NewDraft {
            cc: vec![mailbox("carol@example.org")],
            body_html: Some("<p>original</p>".to_owned()),
            ..fx.new_draft()
        })
        .await
        .unwrap();

    let updated = store
        .update(
            created.id,
            DraftPatch {
                subject: Some("Dinner".to_owned()),
                ..DraftPatch::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.subject, "Dinner");
    assert_eq!(updated.body_text, created.body_text, "left alone");
    assert_eq!(updated.body_html, created.body_html, "left alone");
    assert_eq!(updated.to, created.to, "left alone");
    assert_eq!(updated.cc, created.cc, "left alone");
    assert_eq!(updated.created_at, created.created_at);
}

#[tokio::test]
async fn an_update_can_clear_the_html_alternative() {
    let fx = Fixture::open();
    let store = fx.store();
    let created = store
        .create(NewDraft {
            body_html: Some("<p>original</p>".to_owned()),
            ..fx.new_draft()
        })
        .await
        .unwrap();

    // The documented way to say "no HTML" through a patch whose absent fields
    // already mean "leave alone".
    let updated = store
        .update(
            created.id,
            DraftPatch {
                body_html: Some(String::new()),
                ..DraftPatch::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.body_html, None);
}

#[tokio::test]
async fn replacing_a_recipient_list_replaces_it_wholesale() {
    let fx = Fixture::open();
    let store = fx.store();
    let created = store
        .create(NewDraft {
            to: vec![mailbox("bob@example.net"), mailbox("dave@example.net")],
            cc: vec![mailbox("carol@example.org")],
            ..fx.new_draft()
        })
        .await
        .unwrap();

    let updated = store
        .update(
            created.id,
            DraftPatch {
                to: Some(vec![mailbox("erin@example.net")]),
                ..DraftPatch::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        updated.to.iter().map(Mailbox::address).collect::<Vec<_>>(),
        vec!["erin@example.net"]
    );
    assert_eq!(
        updated.cc.iter().map(Mailbox::address).collect::<Vec<_>>(),
        vec!["carol@example.org"],
        "an untouched list survives the rewrite of its siblings"
    );
}

#[tokio::test]
async fn clearing_to_is_fine_while_cc_still_carries_the_message() {
    let fx = Fixture::open();
    let store = fx.store();
    let created = store
        .create(NewDraft {
            cc: vec![mailbox("carol@example.org")],
            ..fx.new_draft()
        })
        .await
        .unwrap();

    let updated = store
        .update(
            created.id,
            DraftPatch {
                to: Some(Vec::new()),
                ..DraftPatch::default()
            },
        )
        .await
        .unwrap();
    assert!(updated.to.is_empty());
    assert_eq!(updated.cc.len(), 1);
}

#[tokio::test]
async fn deleting_a_draft_takes_its_recipients_and_attachments_with_it() {
    let fx = Fixture::open();
    let store = fx.store();
    let created = store
        .create(NewDraft {
            attachments: vec![NewAttachment {
                filename: "a.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                content: b"bytes".to_vec(),
            }],
            ..fx.new_draft()
        })
        .await
        .unwrap();

    store.delete(created.id).await.unwrap();
    assert_eq!(
        store.get(created.id).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );

    // The ON DELETE CASCADE, verified against the tables rather than the API.
    let (recipients, attachments): (i64, i64) = fx
        .db
        .with_read(|c| {
            Ok((
                c.query_row("SELECT count(*) FROM draft_recipients", [], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM draft_attachments", [], |r| r.get(0))?,
            ))
        })
        .unwrap();
    assert_eq!((recipients, attachments), (0, 0));
}

#[tokio::test]
async fn deleting_an_account_deletes_its_drafts() {
    let fx = Fixture::open();
    let store = fx.store();
    let created = store.create(fx.new_draft()).await.unwrap();

    let account_id = fx.account_id;
    fx.db
        .with_write(move |c| c.execute("DELETE FROM accounts WHERE id = ?1", [account_id]))
        .unwrap();

    assert_eq!(
        store.get(created.id).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn editing_or_deleting_a_missing_draft_is_not_found() {
    let fx = Fixture::open();
    let store = fx.store();

    assert_eq!(
        store.get(999).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );
    assert_eq!(
        store
            .update(
                999,
                DraftPatch {
                    subject: Some("x".to_owned()),
                    ..DraftPatch::default()
                }
            )
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::NotFound
    );
    assert_eq!(
        store.delete(999).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );
    assert_eq!(
        store.render(999).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );
}

#[tokio::test]
async fn a_draft_with_no_recipients_is_invalid_argument() {
    let fx = Fixture::open();
    let store = fx.store();

    let err = store
        .create(NewDraft {
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            ..fx.new_draft()
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);

    // And an edit cannot get there either.
    let created = store.create(fx.new_draft()).await.unwrap();
    let err = store
        .update(
            created.id,
            DraftPatch {
                to: Some(Vec::new()),
                ..DraftPatch::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert_eq!(
        store.get(created.id).await.unwrap().to.len(),
        1,
        "a rejected edit rolls back rather than half-applying"
    );
}

#[tokio::test]
async fn an_unparseable_address_never_reaches_the_database() {
    // `Mailbox` is the gate — the store cannot be handed a bad address
    // because there is no way to construct one.
    for bad in [
        "not-an-address",
        "two@at@signs.com",
        "alice@example.com>\r\nBcc: victim@example.org",
        "älice@example.com",
    ] {
        assert_eq!(
            Mailbox::parse(bad).unwrap_err().reason(),
            ErrorReason::InvalidArgument,
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn a_subject_with_a_control_character_is_invalid_argument() {
    let fx = Fixture::open();
    let err = fx
        .store()
        .create(NewDraft {
            subject: "Hi\r\nBcc: victim@example.org".to_owned(),
            ..fx.new_draft()
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn an_oversized_subject_is_invalid_argument() {
    let fx = Fixture::open();
    let err = fx
        .store()
        .create(NewDraft {
            subject: "x".repeat(MAX_SUBJECT + 1),
            ..fx.new_draft()
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn oversized_attachments_are_resource_exhausted() {
    let fx = Fixture::open();
    let err = fx
        .store()
        .create(NewDraft {
            attachments: vec![NewAttachment {
                filename: "huge.bin".to_owned(),
                content_type: "application/octet-stream".to_owned(),
                content: vec![0u8; MAX_ATTACHMENT_BYTES + 1],
            }],
            ..fx.new_draft()
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::ResourceExhausted);
}

#[tokio::test]
async fn too_many_attachments_is_resource_exhausted() {
    // `MAX_ATTACHMENT_BYTES` counts content only, so without a count bound
    // this draft — 51 empty attachments — costs nothing against it while
    // still adding 51 rows and 51 MIME parts.
    let fx = Fixture::open();
    let err = fx
        .store()
        .create(NewDraft {
            attachments: (0..=MAX_ATTACHMENTS)
                .map(|n| NewAttachment {
                    filename: format!("a{n}.txt"),
                    content_type: "text/plain".to_owned(),
                    content: Vec::new(),
                })
                .collect(),
            ..fx.new_draft()
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::ResourceExhausted);
}

#[tokio::test]
async fn an_oversized_content_type_is_invalid_argument() {
    // `Content-Type` is one unfoldable header token, so an unbounded value
    // would render as a line past RFC 5322's 998-octet limit — an
    // `INTERNAL` on a draft that had already been accepted.
    let fx = Fixture::open();
    let err = fx
        .store()
        .create(NewDraft {
            attachments: vec![NewAttachment {
                filename: "a.bin".to_owned(),
                content_type: format!("{}/{}", "a".repeat(MAX_CONTENT_TYPE), "b"),
                content: b"x".to_vec(),
            }],
            ..fx.new_draft()
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn an_attachment_filename_is_reduced_to_its_basename() {
    // A receiving client that honours `filename` verbatim must not be handed
    // a path to write to.
    let fx = Fixture::open();
    let draft = fx
        .store()
        .create(NewDraft {
            attachments: vec![NewAttachment {
                filename: "../../.ssh/authorized_keys".to_owned(),
                content_type: "text/plain".to_owned(),
                content: b"x".to_vec(),
            }],
            ..fx.new_draft()
        })
        .await
        .unwrap();
    assert_eq!(draft.attachments[0].filename, "authorized_keys");
}

#[tokio::test]
async fn a_nameless_or_control_bearing_filename_is_invalid_argument() {
    let fx = Fixture::open();
    for filename in ["", "   ", "..", "dir/", "a\r\nb.txt"] {
        let err = fx
            .store()
            .create(NewDraft {
                attachments: vec![NewAttachment {
                    filename: filename.to_owned(),
                    content_type: "text/plain".to_owned(),
                    content: b"x".to_vec(),
                }],
                ..fx.new_draft()
            })
            .await
            .unwrap_err();
        assert_eq!(err.reason(), ErrorReason::InvalidArgument, "{filename:?}");
    }
}

#[tokio::test]
async fn a_draft_on_a_missing_account_or_parent_is_not_found() {
    let fx = Fixture::open();
    let store = fx.store();

    assert_eq!(
        store
            .create(NewDraft {
                account_id: 9999,
                ..fx.new_draft()
            })
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::NotFound
    );
    assert_eq!(
        store
            .create(NewDraft {
                in_reply_to_message_id: Some(9999),
                ..fx.new_draft()
            })
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::NotFound
    );
}

#[tokio::test]
async fn a_reply_cannot_thread_onto_another_accounts_message() {
    // Without account scoping in `resolve_threading` this succeeds, and the
    // other account's Message-ID ends up in this message's References — sent
    // to every recipient, and cross-linking two mailboxes that were meant to
    // stay separate.
    let fx = Fixture::open();
    let other_account = fx
        .db
        .with_write(|c| {
            repo::insert_account(
                c,
                &repo::NewAccount {
                    name: "Work".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let parent = fx.message(Some("work-secret@example.com"), None, None);

    let err = fx
        .store()
        .create(NewDraft {
            // The parent belongs to `fx.account_id`; the draft does not.
            account_id: other_account,
            in_reply_to_message_id: Some(parent),
            ..fx.new_draft()
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

// ---------------------------------------------------------------------------
// Threading
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_reply_freezes_the_parents_threading_headers() {
    let fx = Fixture::open();
    let store = fx.store();
    let parent = fx.message(
        Some("parent@example.com"),
        Some("root@example.com"),
        Some("root@example.com grandparent@example.com"),
    );

    let draft = store
        .create(NewDraft {
            in_reply_to_message_id: Some(parent),
            ..fx.new_draft()
        })
        .await
        .unwrap();

    assert_eq!(draft.in_reply_to.as_deref(), Some("parent@example.com"));
    assert_eq!(
        draft.references,
        vec![
            "root@example.com".to_owned(),
            "grandparent@example.com".to_owned(),
            "parent@example.com".to_owned()
        ],
        "the parent's References plus the parent's own id (RFC 5322 §3.6.4)"
    );
}

#[tokio::test]
async fn a_reply_to_a_thread_root_still_gets_a_references_chain() {
    // The parent has no References of its own; dropping its id would start a
    // second conversation rather than continuing this one.
    let fx = Fixture::open();
    let parent = fx.message(Some("root@example.com"), None, None);
    let draft = fx
        .store()
        .create(NewDraft {
            in_reply_to_message_id: Some(parent),
            ..fx.new_draft()
        })
        .await
        .unwrap();

    assert_eq!(draft.in_reply_to.as_deref(), Some("root@example.com"));
    assert_eq!(draft.references, vec!["root@example.com".to_owned()]);
}

#[tokio::test]
async fn a_reply_to_a_parent_with_only_in_reply_to_keeps_the_grandparent() {
    let fx = Fixture::open();
    let parent = fx.message(
        Some("parent@example.com"),
        Some("grandparent@example.com"),
        None,
    );
    let draft = fx
        .store()
        .create(NewDraft {
            in_reply_to_message_id: Some(parent),
            ..fx.new_draft()
        })
        .await
        .unwrap();

    assert_eq!(
        draft.references,
        vec![
            "grandparent@example.com".to_owned(),
            "parent@example.com".to_owned()
        ]
    );
}

#[tokio::test]
async fn a_reply_to_a_parent_with_no_message_id_has_no_in_reply_to() {
    let fx = Fixture::open();
    let parent = fx.message(None, None, Some("root@example.com"));
    let draft = fx
        .store()
        .create(NewDraft {
            in_reply_to_message_id: Some(parent),
            ..fx.new_draft()
        })
        .await
        .unwrap();

    assert_eq!(draft.in_reply_to, None);
    assert_eq!(draft.references, vec!["root@example.com".to_owned()]);
}

#[tokio::test]
async fn losing_the_parent_does_not_lose_the_frozen_threading_headers() {
    // The whole reason the headers are frozen: `in_reply_to_message_id` is
    // ON DELETE SET NULL, and a reply that silently detaches from its
    // conversation between being written and being sent is invisible to the
    // person who wrote it.
    let fx = Fixture::open();
    let store = fx.store();
    let parent = fx.message(Some("parent@example.com"), None, Some("root@example.com"));
    let draft = store
        .create(NewDraft {
            in_reply_to_message_id: Some(parent),
            ..fx.new_draft()
        })
        .await
        .unwrap();

    fx.db
        .with_write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [parent]))
        .unwrap();

    let after = store.get(draft.id).await.unwrap();
    assert_eq!(after.in_reply_to_message_id, None, "the link is gone");
    assert_eq!(
        after.in_reply_to.as_deref(),
        Some("parent@example.com"),
        "but the header is not"
    );
    assert_eq!(
        after.references,
        vec![
            "root@example.com".to_owned(),
            "parent@example.com".to_owned()
        ]
    );

    let rendered = store.render(draft.id).await.unwrap();
    let parsed = parse_message(&rendered.mime);
    assert_eq!(parsed.in_reply_to.as_deref(), Some("parent@example.com"));
    assert_eq!(
        parsed.references.as_deref(),
        Some("root@example.com parent@example.com")
    );
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn render_produces_a_parseable_message_and_the_full_rcpt_to_list() {
    let fx = Fixture::open();
    let draft = fx
        .store()
        .create(NewDraft {
            to: vec![mailbox("bob@example.net")],
            cc: vec![mailbox("carol@example.org")],
            bcc: vec![mailbox("secret@example.org")],
            subject: "Café".to_owned(),
            attachments: vec![NewAttachment {
                filename: "a.txt".to_owned(),
                content_type: "text/plain".to_owned(),
                content: b"attached bytes".to_vec(),
            }],
            ..fx.new_draft()
        })
        .await
        .unwrap();

    let rendered = fx.store().render(draft.id).await.unwrap();
    let parsed = parse_message(&rendered.mime);

    assert_eq!(parsed.subject.as_deref(), Some("Café"));
    assert_eq!(
        parsed.message_id.as_deref(),
        Some(rendered.message_id.as_str())
    );
    assert!(
        rendered.message_id.ends_with("@example.com"),
        "the sender's domain"
    );
    assert_eq!(parsed.attachments.len(), 1);

    // Bcc reaches the envelope and only the envelope.
    assert_eq!(
        rendered.envelope_recipients,
        vec![
            "bob@example.net".to_owned(),
            "carol@example.org".to_owned(),
            "secret@example.org".to_owned()
        ]
    );
    assert!(!String::from_utf8_lossy(&rendered.mime).contains("secret@example.org"));
}

#[tokio::test]
async fn every_render_mints_a_fresh_message_id() {
    // The identity of a *sent* message is minted once by the send path; a
    // preview must not be able to claim an id that was already transmitted.
    let fx = Fixture::open();
    let store = fx.store();
    let draft = store.create(fx.new_draft()).await.unwrap();

    let first = store.render(draft.id).await.unwrap();
    let second = store.render(draft.id).await.unwrap();
    assert_ne!(first.message_id, second.message_id);
}

#[tokio::test]
async fn a_duplicate_address_appears_once_in_the_envelope() {
    let fx = Fixture::open();
    let draft = fx
        .store()
        .create(NewDraft {
            to: vec![mailbox("bob@example.net")],
            cc: vec![mailbox("Bob Again <bob@example.net>")],
            ..fx.new_draft()
        })
        .await
        .unwrap();
    assert_eq!(
        draft.envelope_recipients(),
        vec!["bob@example.net".to_owned()],
        "one RCPT TO per address, or the recipient gets two copies"
    );
}
