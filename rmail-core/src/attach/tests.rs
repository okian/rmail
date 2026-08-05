//! The pipeline: what lands in `index_content`, what is skipped, and what
//! happens to rows whose attachment is gone.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::repo;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A multipart RFC822 message carrying the given attachments.
fn message_with(attachments: &[(&str, &str, &[u8])]) -> Vec<u8> {
    use std::fmt::Write;
    let mut out = String::from(
        "From: ada@example.com\r\n\
         To: bob@example.com\r\n\
         Subject: With attachments\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n\r\n\
         --BOUND\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n\
         Please see the attached.\r\n",
    );
    for (filename, content_type, bytes) in attachments {
        let encoded = base64(bytes);
        let _ = write!(
            out,
            "--BOUND\r\n\
             Content-Type: {content_type}\r\n\
             Content-Disposition: attachment; filename=\"{filename}\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n\
             {encoded}\r\n"
        );
    }
    out.push_str("--BOUND--\r\n");
    out.into_bytes()
}

/// Base64, wrapped, because a mail transfer encoding is part of the fixture.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for (n, block) in bytes.chunks(3).enumerate() {
        if n > 0 && n % 19 == 0 {
            out.push_str("\r\n");
        }
        let b = [
            *block.first().unwrap_or(&0),
            *block.get(1).unwrap_or(&0),
            *block.get(2).unwrap_or(&0),
        ];
        let triple = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= block.len() {
                let index = ((triple >> (18 - i * 6)) & 0x3f) as usize;
                out.push(char::from(ALPHABET[index]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

struct Fixture {
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-attach-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let (account_id, mailbox_id) = db
            .write(|c| {
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
            .await
            .unwrap();
        Self {
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    async fn insert(&self, raw: Option<Vec<u8>>) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        self.db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        raw,
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn set_raw(&self, message_id: i64, raw: Vec<u8>) {
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE messages SET raw = ?2 WHERE id = ?1",
                    rusqlite::params![message_id, raw],
                )
            })
            .await
            .unwrap();
    }

    fn indexed_text(&self, message_id: i64) -> Vec<(String, String)> {
        self.db
            .with_read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT part, text FROM index_content
                     WHERE message_id = ?1 AND part LIKE 'attachment:%' ORDER BY part",
                )?;
                let rows = stmt
                    .query_map([message_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        self.db
            .with_read(move |c| c.query_row(&sql, [], |r| r.get(0)))
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

/// The configuration the product actually ships.
///
/// Deliberately not an override. The suite used to add "html" and drop "eml",
/// which meant every test ran against a config that did not exist in the wild —
/// and the shipped default silently declined every HTML attachment for as long
/// as that was true.
fn config() -> IndexExtractConfig {
    IndexExtractConfig::default()
}

#[tokio::test]
async fn attachment_text_lands_where_the_body_does() {
    // Next to the subject and the body, so the lexical index, the entity
    // extractor and the chunker all reach it through paths they already have.
    let fx = Fixture::open().await;
    let raw = message_with(&[(
        "notes.txt",
        "text/plain",
        b"The quarterly hosting invoice is INV-9.",
    )]);
    let message_id = fx.insert(Some(raw)).await;

    let report = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    assert_eq!(report.attachments, 1);
    assert_eq!(report.extracted, 1);
    let indexed = fx.indexed_text(message_id);
    assert_eq!(indexed.len(), 1);
    assert_eq!(indexed[0].0, "attachment:0");
    assert!(indexed[0].1.contains("INV-9"), "{:?}", indexed[0].1);
}

#[tokio::test]
async fn a_pdf_attachment_records_its_pages() {
    let fx = Fixture::open().await;
    let pdf =
        super::extract::tests::pdf_bytes(&["Page one about invoices", "Page two about hosting"]);
    let raw = message_with(&[("contract.pdf", "application/pdf", &pdf)]);
    let message_id = fx.insert(Some(raw)).await;

    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    let recorded = stored(&fx.db, message_id).await.unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].status, Status::Ok);
    assert_eq!(recorded[0].pages, Some(2));

    // A citation into a fifty-page contract has to name the page, and the only
    // route from a search hit — which knows a byte offset — is this.
    let text = fx.indexed_text(message_id);
    let at = text[0].1.find("hosting").expect("the second page's text");
    assert_eq!(
        page_at(&fx.db, message_id, "0", at as i64).await.unwrap(),
        Some(2),
        "an offset on page two must resolve to page two"
    );
    assert_eq!(page_at(&fx.db, message_id, "0", 0).await.unwrap(), Some(1));
}

#[tokio::test]
async fn re_extracting_unchanged_attachments_does_no_work() {
    // The queue redelivers on lease expiry, so this is the common case. A PDF
    // parse is the most expensive no-op in the indexer.
    let fx = Fixture::open().await;
    let pdf = super::extract::tests::pdf_bytes(&["Invoice INV-9"]);
    let raw = message_with(&[("a.pdf", "application/pdf", &pdf)]);
    let message_id = fx.insert(Some(raw)).await;
    let first = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(first.extracted, 1);
    assert_eq!(first.unchanged, 0);

    let second = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    assert_eq!(second.unchanged, 1);
    assert_eq!(second.extracted, 0);
    // And the text is still there — skipping must not mean deleting.
    assert_eq!(fx.indexed_text(message_id).len(), 1);
}

#[tokio::test]
async fn an_oversized_attachment_is_recorded_rather_than_read() {
    // The point of the limit is to not read the file, and detection reads it.
    let fx = Fixture::open().await;
    let big = vec![b'x'; 3 * 1024 * 1024];
    let raw = message_with(&[("huge.txt", "text/plain", &big)]);
    let message_id = fx.insert(Some(raw)).await;

    let report = extract_attachments(
        &fx.db,
        &IndexExtractConfig {
            max_attachment_mb: 1,
            ..config()
        },
        message_id,
    )
    .await
    .unwrap();

    assert_eq!(report.empty, 1);
    assert_eq!(report.extracted, 0);
    let recorded = stored(&fx.db, message_id).await.unwrap();
    assert_eq!(recorded[0].status, Status::TooLarge);
    assert_eq!(recorded[0].bytes, 3 * 1024 * 1024);
    assert!(fx.indexed_text(message_id).is_empty());
}

#[tokio::test]
async fn an_unsupported_attachment_is_recorded_and_not_retried() {
    // Without a row saying so it is indistinguishable from "not done yet", and
    // the pipeline would re-open it on every pass for the life of the mailbox.
    let fx = Fixture::open().await;
    let raw = message_with(&[("photo.jpg", "image/jpeg", &[0xff, 0xd8, 0xff, 0xe0, 0, 0])]);
    let message_id = fx.insert(Some(raw)).await;

    let first = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(first.empty, 1);
    assert_eq!(
        stored(&fx.db, message_id).await.unwrap()[0].status,
        Status::Unsupported
    );

    let second = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(second.unchanged, 1, "it must not be reconsidered");
}

#[tokio::test]
async fn a_format_the_operator_turned_off_is_left_alone() {
    let fx = Fixture::open().await;
    let pdf = super::extract::tests::pdf_bytes(&["Invoice INV-9"]);
    let raw = message_with(&[("a.pdf", "application/pdf", &pdf)]);
    let message_id = fx.insert(Some(raw)).await;

    extract_attachments(
        &fx.db,
        &IndexExtractConfig {
            formats: vec!["txt".to_owned()],
            ..config()
        },
        message_id,
    )
    .await
    .unwrap();

    let recorded = stored(&fx.db, message_id).await.unwrap();
    assert_eq!(recorded[0].status, Status::Unsupported);
    // The extractor name records which format it *would* have used, so turning
    // it back on can find the rows worth redoing.
    assert_eq!(recorded[0].extractor, "pdf-extract/0.12");
    assert!(fx.indexed_text(message_id).is_empty());
}

#[tokio::test]
async fn disabling_attachments_entirely_is_a_no_op() {
    let fx = Fixture::open().await;
    let raw = message_with(&[("notes.txt", "text/plain", b"hello")]);
    let message_id = fx.insert(Some(raw)).await;

    let report = extract_attachments(
        &fx.db,
        &IndexExtractConfig {
            attachments: false,
            ..config()
        },
        message_id,
    )
    .await
    .unwrap();

    assert_eq!(
        report,
        AttachReport {
            message_id,
            ..AttachReport::default()
        }
    );
    assert_eq!(fx.count("attachment_extractions"), 0);
}

#[tokio::test]
async fn an_attachment_that_changed_stops_being_searchable_by_its_old_text() {
    // A re-fetch after a `UIDVALIDITY` rebuild replaces the raw. Text from the
    // version that is gone must go with it.
    let fx = Fixture::open().await;
    let message_id = fx
        .insert(Some(message_with(&[(
            "notes.txt",
            "text/plain",
            b"The original text mentions kingfishers.",
        )])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert!(fx.indexed_text(message_id)[0].1.contains("kingfishers"));

    fx.set_raw(
        message_id,
        message_with(&[(
            "notes.txt",
            "text/plain",
            b"The replacement mentions herons.",
        )]),
    )
    .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    let indexed = fx.indexed_text(message_id);
    assert_eq!(indexed.len(), 1);
    assert!(!indexed[0].1.contains("kingfishers"), "{:?}", indexed[0].1);
    assert!(indexed[0].1.contains("herons"));
}

#[tokio::test]
async fn an_attachment_that_disappeared_takes_its_rows_with_it() {
    let fx = Fixture::open().await;
    let message_id = fx
        .insert(Some(message_with(&[
            ("one.txt", "text/plain", b"First attachment about invoices."),
            ("two.txt", "text/plain", b"Second attachment about hosting."),
        ])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(fx.indexed_text(message_id).len(), 2);

    fx.set_raw(
        message_id,
        message_with(&[("one.txt", "text/plain", b"First attachment about invoices.")]),
    )
    .await;
    let report = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    assert_eq!(report.removed, 1);
    assert_eq!(fx.indexed_text(message_id).len(), 1);
    assert_eq!(fx.count("attachment_extractions"), 1);
    assert_eq!(fx.count("attachment_pages"), 0);
}

#[tokio::test]
async fn an_attachment_that_now_yields_nothing_loses_its_old_text() {
    // Swapped for an encrypted or corrupt version. The status changes and the
    // text has to go with it, or the message stays searchable by contents it
    // no longer has.
    let fx = Fixture::open().await;
    let message_id = fx
        .insert(Some(message_with(&[(
            "a.txt",
            "text/plain",
            b"Findable text about invoices.",
        )])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(fx.indexed_text(message_id).len(), 1);

    fx.set_raw(
        message_id,
        message_with(&[("a.txt", "text/plain", b"   \n  ")]),
    )
    .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    assert!(fx.indexed_text(message_id).is_empty());
    assert_eq!(
        stored(&fx.db, message_id).await.unwrap()[0].status,
        Status::Empty
    );
}

#[tokio::test]
async fn a_config_change_reconsiders_exactly_what_it_would_have_changed() {
    // Without this, re-enabling a format left every previously-declined
    // attachment permanently declined: the bytes had not changed, so nothing
    // was reconsidered, and `Unsupported` is not retryable so no repair path
    // existed either.
    let fx = Fixture::open().await;
    let pdf = super::extract::tests::pdf_bytes(&["Invoice INV-9"]);
    let message_id = fx
        .insert(Some(message_with(&[("a.pdf", "application/pdf", &pdf)])))
        .await;

    let off = IndexExtractConfig {
        formats: vec!["txt".to_owned()],
        ..config()
    };
    extract_attachments(&fx.db, &off, message_id).await.unwrap();
    assert!(fx.indexed_text(message_id).is_empty());
    // Unchanged config, unchanged bytes: nothing is reconsidered.
    let again = extract_attachments(&fx.db, &off, message_id).await.unwrap();
    assert_eq!(again.unchanged, 1);

    let back_on = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(back_on.unchanged, 0, "the format list changed");
    assert_eq!(back_on.extracted, 1);
    assert!(fx.indexed_text(message_id)[0].1.contains("INV-9"));
}

#[tokio::test]
async fn raising_the_size_limit_reconsiders_what_it_declined() {
    let fx = Fixture::open().await;
    let big = vec![b'x'; 2 * 1024 * 1024];
    let message_id = fx
        .insert(Some(message_with(&[("huge.txt", "text/plain", &big)])))
        .await;

    extract_attachments(
        &fx.db,
        &IndexExtractConfig {
            max_attachment_mb: 1,
            ..config()
        },
        message_id,
    )
    .await
    .unwrap();
    assert_eq!(
        stored(&fx.db, message_id).await.unwrap()[0].status,
        Status::TooLarge
    );

    let raised = extract_attachments(
        &fx.db,
        &IndexExtractConfig {
            max_attachment_mb: 5,
            ..config()
        },
        message_id,
    )
    .await
    .unwrap();
    assert_eq!(raised.unchanged, 0);
    assert_eq!(raised.extracted, 1);
}

#[tokio::test]
async fn an_unparsable_raw_does_not_delete_the_work_already_done() {
    // An empty part list from a failed parse used to read as "every attachment
    // is gone", dropping every row this message has — including minutes of
    // extraction — on the strength of one unparsable byte. A mail-parser
    // upgrade that changes what counts as an attachment would do it at scale.
    let fx = Fixture::open().await;
    let message_id = fx
        .insert(Some(message_with(&[(
            "a.txt",
            "text/plain",
            b"Findable text about invoices.",
        )])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(fx.count("attachment_extractions"), 1);

    fx.set_raw(message_id, Vec::new()).await;
    let report = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    assert_eq!(report.removed, 0);
    assert_eq!(fx.count("attachment_extractions"), 1, "rows were dropped");
    assert_eq!(fx.indexed_text(message_id).len(), 1);
}

#[tokio::test]
async fn an_oversized_attachment_is_never_copied_or_hashed() {
    // The limit's whole point is not handling the file. A test that only checks
    // the recorded status passes just as well when the attachment is fully
    // extracted and the result then thrown away.
    let fx = Fixture::open().await;
    let pdf = super::extract::tests::pdf_bytes(&["Invoice INV-9 is findable"]);
    let message_id = fx
        .insert(Some(message_with(&[("a.pdf", "application/pdf", &pdf)])))
        .await;

    extract_attachments(
        &fx.db,
        &IndexExtractConfig {
            max_attachment_mb: 0,
            ..config()
        },
        message_id,
    )
    .await
    .unwrap();

    let recorded = stored(&fx.db, message_id).await.unwrap();
    assert_eq!(recorded[0].status, Status::TooLarge);
    assert_eq!(recorded[0].extractor, "size-limit");
    assert_eq!(recorded[0].chars, 0);
    assert!(
        fx.indexed_text(message_id).is_empty(),
        "the file was read after all"
    );
    assert_eq!(fx.count("attachment_pages"), 0);
}

#[tokio::test]
async fn a_re_extraction_that_loses_pages_drops_the_old_page_rows() {
    let fx = Fixture::open().await;
    let three = super::extract::tests::pdf_bytes(&["Alpha page", "Bravo page", "Charlie page"]);
    let message_id = fx
        .insert(Some(message_with(&[("a.pdf", "application/pdf", &three)])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(fx.count("attachment_pages"), 3);

    let one = super::extract::tests::pdf_bytes(&["Alpha page only now"]);
    fx.set_raw(
        message_id,
        message_with(&[("a.pdf", "application/pdf", &one)]),
    )
    .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    assert_eq!(
        fx.count("attachment_pages"),
        1,
        "page rows from the previous version survived"
    );
    assert_eq!(page_at(&fx.db, message_id, "0", 0).await.unwrap(), Some(1));
}

#[tokio::test]
async fn the_indexed_hash_moves_when_the_text_does_not_match_the_bytes() {
    // `index::extract::message_hash` folds every row's `content_hash` into the
    // message-level re-index gate. Storing the attachment's *byte* hash there
    // means a better extractor producing different text from identical bytes
    // never triggers a re-chunk or a re-embed.
    let fx = Fixture::open().await;
    let message_id = fx
        .insert(Some(message_with(&[(
            "a.txt",
            "text/plain",
            b"Findable text about invoices.",
        )])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    let (text, hash): (String, Vec<u8>) = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT text, content_hash FROM index_content
                 WHERE message_id = ?1 AND part = 'attachment:0'",
                [message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(
        hash,
        Sha256::digest(text.as_bytes()).to_vec(),
        "the stored hash must describe the stored text"
    );
}

#[tokio::test]
async fn a_message_with_no_stored_body_is_a_failed_precondition() {
    // Recorded from an IMAP envelope without a body fetch: there is nothing to
    // extract from, and saying so is more useful than reporting zero
    // attachments on a message that has several.
    let fx = Fixture::open().await;
    let message_id = fx.insert(None).await;

    let err = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
}

#[tokio::test]
async fn a_message_with_no_attachments_is_not_an_error() {
    let fx = Fixture::open().await;
    let message_id = fx.insert(Some(message_with(&[]))).await;

    let report = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(report.attachments, 0);
    assert_eq!(fx.count("attachment_extractions"), 0);
}

#[tokio::test]
async fn deleting_a_message_takes_its_attachment_rows_with_it() {
    let fx = Fixture::open().await;
    let pdf = super::extract::tests::pdf_bytes(&["Invoice INV-9"]);
    let message_id = fx
        .insert(Some(message_with(&[("a.pdf", "application/pdf", &pdf)])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(fx.count("attachment_pages"), 1);

    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [message_id]))
        .await
        .unwrap();

    assert_eq!(fx.count("attachment_extractions"), 0);
    assert_eq!(fx.count("attachment_pages"), 0);
}

#[tokio::test]
async fn attachment_text_reaches_the_lexical_index() {
    // The whole point: a term that appears only in a PDF has to be findable.
    let fx = Fixture::open().await;
    let pdf = super::extract::tests::pdf_bytes(&["The termination for convenience clause"]);
    let message_id = fx
        .insert(Some(message_with(&[(
            "contract.pdf",
            "application/pdf",
            &pdf,
        )])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    let fts =
        crate::index::fts::FtsIndex::new(fx.db.clone(), crate::config::Bm25Weights::default());
    assert!(fts.index_message(message_id).await.unwrap());

    let hits = fts.search("convenience", 10).await.unwrap();
    assert_eq!(hits.len(), 1, "the PDF's text is not searchable");
    assert_eq!(hits[0].message_id, message_id);
}

#[tokio::test]
async fn a_failed_extraction_is_retried_only_under_a_different_extractor() {
    // The same extractor over the same bytes fails the same way; retrying it is
    // a loop with extra steps.
    let fx = Fixture::open().await;
    let message_id = fx
        .insert(Some(message_with(&[("a.txt", "text/plain", b"hello")])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE attachment_extractions SET status = 'failed', extractor = 'text/1'
                 WHERE message_id = ?1",
                [message_id],
            )
        })
        .await
        .unwrap();
    assert!(
        retryable(&fx.db, 10).await.unwrap().is_empty(),
        "the current extractor would fail the same way"
    );

    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE attachment_extractions SET extractor = 'text/0' WHERE message_id = ?1",
                [message_id],
            )
        })
        .await
        .unwrap();
    assert_eq!(retryable(&fx.db, 10).await.unwrap(), vec![message_id]);
}

#[tokio::test]
async fn a_failed_attachment_is_reconsidered_on_the_next_pass() {
    // Unlike every other non-Ok status: a failure may be a bug that a later
    // build fixed, so the skip decision must not treat it as settled.
    let fx = Fixture::open().await;
    let message_id = fx
        .insert(Some(message_with(&[(
            "a.txt",
            "text/plain",
            b"Findable text about invoices.",
        )])))
        .await;
    extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();

    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE attachment_extractions SET status = 'failed' WHERE message_id = ?1",
                [message_id],
            )
        })
        .await
        .unwrap();

    let report = extract_attachments(&fx.db, &config(), message_id)
        .await
        .unwrap();
    assert_eq!(report.unchanged, 0, "a failure is not settled");
    assert_eq!(report.extracted, 1);
}
