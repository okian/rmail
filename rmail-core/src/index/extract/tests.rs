//! What extraction owes the stages downstream of it: parts that mean something
//! on their own, a normalized form the indexes agree on, and a hash that
//! changes when and only when something searchable did.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::index::{IndexKind, QueueOptions};
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-extract-{pid}-{n}.db"));
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
        let queue = IndexQueue::new(db.clone(), QueueOptions::default());
        Self {
            db,
            queue,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    async fn store(&self, new: repo::NewMessage) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            ..new
        };
        self.db
            .write(move |c| repo::insert_message(c, &new))
            .await
            .unwrap()
    }

    /// Replace a stored message's body, as a re-fetch or an edit would.
    async fn set_body(&self, message_id: i64, body: Option<&str>) {
        let body = body.map(str::to_owned);
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE messages SET body_text = ?2 WHERE id = ?1",
                    rusqlite::params![message_id, body],
                )
            })
            .await
            .unwrap();
    }

    fn content(&self, message_id: i64) -> Vec<(String, String, Option<String>, i64)> {
        self.db
            .with_read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT part, text, lang, chars FROM index_content
                     WHERE message_id = ?1 ORDER BY part",
                )?;
                let rows = stmt
                    .query_map([message_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
    }

    fn text_of(&self, message_id: i64, part: &str) -> Option<String> {
        self.content(message_id)
            .into_iter()
            .find(|(key, _, _, _)| key == part)
            .map(|(_, text, _, _)| text)
    }

    async fn extract(&self, message_id: i64) -> ExtractReport {
        extract_message(&self.db, &self.queue, message_id, PRIORITY_NORMAL)
            .await
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

#[tokio::test]
async fn a_message_becomes_one_row_per_meaningful_part() {
    // A subject, a participant line and a body are different documents with
    // different weight in ranking. Merging them would throw away the only
    // signal the ranker has about where a match came from.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Quarterly review".to_owned()),
            from_name: Some("Ada Lovelace".to_owned()),
            from_addr: Some("ada@example.com".to_owned()),
            to_addrs: Some("me@example.com".to_owned()),
            body_text: Some("The numbers are attached.".to_owned()),
            ..Default::default()
        })
        .await;

    let report = fx.extract(message_id).await;

    assert_eq!(
        report.written,
        vec![Part::Subject, Part::Headers, Part::Body]
    );
    let content = fx.content(message_id);
    assert_eq!(content.len(), 3);
    assert_eq!(
        fx.text_of(message_id, "subject").unwrap(),
        "Quarterly review"
    );
    assert_eq!(
        fx.text_of(message_id, "headers").unwrap(),
        "Ada Lovelace ada@example.com me@example.com",
        "one searchable line, so a person matches whether they sent or received"
    );
    assert_eq!(
        fx.text_of(message_id, "body").unwrap(),
        "The numbers are attached."
    );
}

#[tokio::test]
async fn a_second_extraction_of_unchanged_mail_writes_nothing() {
    // The common case: a sync sweep re-enqueues the world on every restart.
    // If that cost a rewrite rather than a hash, starting the daemon would
    // re-index the mailbox.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Hello".to_owned()),
            body_text: Some("A body long enough to be interesting.".to_owned()),
            ..Default::default()
        })
        .await;

    let first = fx.extract(message_id).await;
    assert!(first.changed());

    let second = fx.extract(message_id).await;

    assert!(second.written.is_empty());
    assert!(second.removed.is_empty());
    assert!(!second.changed());
    assert_eq!(second.unchanged.len(), first.written.len());
    assert_eq!(
        second.content_hash, first.content_hash,
        "and the hash the later stages dedup on is identical"
    );
    assert_eq!(
        second.follow_on, 0,
        "so nothing downstream is queued either"
    );
}

#[tokio::test]
async fn changed_text_rewrites_only_the_part_that_changed() {
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Stable subject".to_owned()),
            from_addr: Some("ada@example.com".to_owned()),
            body_text: Some("Version one of the body.".to_owned()),
            ..Default::default()
        })
        .await;
    let first = fx.extract(message_id).await;

    fx.set_body(message_id, Some("Version two of the body."))
        .await;
    let second = fx.extract(message_id).await;

    assert_eq!(second.written, vec![Part::Body]);
    assert_eq!(
        second.unchanged.len(),
        2,
        "subject and headers were left alone"
    );
    assert_ne!(second.content_hash, first.content_hash);
    assert_eq!(
        fx.text_of(message_id, "body").unwrap(),
        "Version two of the body."
    );
}

#[tokio::test]
async fn reformatting_that_changes_nothing_searchable_does_not_re_index() {
    // The reason the hash is over the *normalized* text. A client that rewraps
    // a body, or a server that returns CRLF where it once returned LF, has
    // changed nothing — and re-embedding a hundred thousand messages to learn
    // that would be an expensive lesson.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            body_text: Some("The quick brown fox\njumps over the lazy dog.".to_owned()),
            ..Default::default()
        })
        .await;
    let first = fx.extract(message_id).await;

    fx.set_body(
        message_id,
        Some("The   quick brown fox\r\n\r\n jumps over   the lazy dog.  "),
    )
    .await;
    let second = fx.extract(message_id).await;

    assert!(second.written.is_empty(), "nothing searchable changed");
    assert_eq!(second.content_hash, first.content_hash);
    assert_eq!(second.follow_on, 0);
}

#[tokio::test]
async fn html_is_stripped_when_that_is_all_there_is() {
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            body_html: Some(
                "<html><body><p>Hello <b>there</b>.</p><p>Second line.</p></body></html>"
                    .to_owned(),
            ),
            ..Default::default()
        })
        .await;

    fx.extract(message_id).await;

    let body = fx.text_of(message_id, "body").unwrap();
    assert!(!body.contains('<'), "no markup survives: {body}");
    assert!(body.contains("Hello"), "{body}");
    assert!(body.contains("there"), "{body}");
    assert!(body.contains("Second line."), "{body}");
}

#[tokio::test]
async fn an_empty_part_is_not_stored() {
    // An empty subject is not a document. A row of empty text would cost an
    // index entry and match nothing.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("   \n\t  ".to_owned()),
            body_text: Some(String::new()),
            from_addr: Some("ada@example.com".to_owned()),
            ..Default::default()
        })
        .await;

    let report = fx.extract(message_id).await;

    assert_eq!(report.written, vec![Part::Headers]);
    assert_eq!(fx.content(message_id).len(), 1);
}

#[tokio::test]
async fn a_message_with_nothing_extractable_produces_nothing() {
    let fx = Fixture::open().await;
    let message_id = fx.store(repo::NewMessage::default()).await;

    let report = fx.extract(message_id).await;

    assert!(report.written.is_empty());
    assert!(fx.content(message_id).is_empty());
    assert!(
        !report.content_hash.is_empty(),
        "an empty extraction still has a stable hash, so the stages dedup on it"
    );
}

#[tokio::test]
async fn a_part_that_disappears_is_removed_from_the_index() {
    // Otherwise an edited draft that loses its body leaves stale text in the
    // index forever — searchable, and matching nothing that exists.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Still here".to_owned()),
            body_text: Some("This body is about to go away.".to_owned()),
            ..Default::default()
        })
        .await;
    let first = fx.extract(message_id).await;
    assert!(fx.text_of(message_id, "body").is_some());

    fx.set_body(message_id, None).await;
    let second = fx.extract(message_id).await;

    assert_eq!(second.removed, vec![Part::Body]);
    assert!(second.changed());
    assert!(fx.text_of(message_id, "body").is_none());
    assert_ne!(
        second.content_hash, first.content_hash,
        "losing a part changes what the later stages should index"
    );
}

#[tokio::test]
async fn a_note_or_summary_survives_an_extraction_that_knows_nothing_about_it() {
    // Those parts belong to other subsystems. An extraction that deleted
    // everything it did not itself produce would wipe a user's note every time
    // the message was re-synced.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Has a note".to_owned()),
            ..Default::default()
        })
        .await;
    fx.extract(message_id).await;
    fx.db
        .write(move |c| {
            c.execute(
                "INSERT INTO index_content
                     (message_id, part, text, chars, content_hash, extractor)
                 VALUES (?1, 'note', 'remember to reply', 17, X'00', 'user')",
                [message_id],
            )
        })
        .await
        .unwrap();

    let report = fx.extract(message_id).await;

    assert!(report.removed.is_empty());
    assert_eq!(
        fx.text_of(message_id, "note").as_deref(),
        Some("remember to reply")
    );
}

#[tokio::test]
async fn extraction_queues_the_stages_that_read_its_output() {
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Index me".to_owned()),
            body_text: Some("Something worth indexing here.".to_owned()),
            ..Default::default()
        })
        .await;

    let report = fx.extract(message_id).await;

    assert_eq!(report.follow_on, 3);
    let mut queued: Vec<IndexKind> = fx
        .queue
        .lease("w", 10)
        .await
        .unwrap()
        .into_iter()
        .map(|lease| lease.kind)
        .collect();
    queued.sort();
    assert_eq!(
        queued,
        vec![IndexKind::Lexical, IndexKind::Entities, IndexKind::Semantic]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
        "the stages that read extracted text, and only those"
    );
}

#[tokio::test]
async fn the_queued_hash_is_the_one_the_stages_dedup_on() {
    // The contract between this stage and the queue: the hash extraction
    // publishes is what a later stage records as indexed, so re-running the
    // whole pipeline over unchanged mail is free end to end.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            body_text: Some("Content that will not change.".to_owned()),
            ..Default::default()
        })
        .await;
    let report = fx.extract(message_id).await;

    let leased = fx.queue.lease("w", 10).await.unwrap();
    assert!(leased
        .iter()
        .all(|lease| lease.content_hash.as_deref() == Some(report.content_hash.as_slice())));
    for lease in &leased {
        assert!(fx.queue.complete(lease, None).await.unwrap());
    }

    // Extract again: unchanged, so nothing is re-queued.
    let again = fx.extract(message_id).await;
    assert_eq!(again.follow_on, 0);
    assert!(fx.queue.lease("w", 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn changed_content_re_queues_the_stages() {
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            body_text: Some("Before.".to_owned()),
            ..Default::default()
        })
        .await;
    fx.extract(message_id).await;
    for lease in fx.queue.lease("w", 10).await.unwrap() {
        fx.queue.complete(&lease, None).await.unwrap();
    }

    fx.set_body(message_id, Some("After, and quite different."))
        .await;
    let report = fx.extract(message_id).await;

    assert_eq!(report.follow_on, 3);
    assert_eq!(fx.queue.lease("w", 10).await.unwrap().len(), 3);
}

#[tokio::test]
async fn extracting_a_message_that_does_not_exist_is_not_found() {
    let fx = Fixture::open().await;
    let err = extract_message(&fx.db, &fx.queue, 9_999, PRIORITY_NORMAL)
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn deleting_a_message_takes_its_extracted_text_with_it() {
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Temporary".to_owned()),
            ..Default::default()
        })
        .await;
    fx.extract(message_id).await;
    assert!(!fx.content(message_id).is_empty());

    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [message_id]))
        .await
        .unwrap();

    assert!(
        fx.content(message_id).is_empty(),
        "orphaned text would stay searchable and match nothing that exists"
    );
}

#[tokio::test]
async fn a_long_body_gets_a_language_and_a_short_subject_does_not() {
    // `None` is a real answer. A two-word subject has no detectable language,
    // and a wrong guess picks the wrong stemmer — which fails to match obvious
    // terms and looks like a broken index rather than a bad guess.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Hi".to_owned()),
            body_text: Some(
                "The quick brown fox jumps over the lazy dog, and then it does \
                 so again because that is what the sentence is for."
                    .to_owned(),
            ),
            ..Default::default()
        })
        .await;

    fx.extract(message_id).await;

    let content = fx.content(message_id);
    let subject = content.iter().find(|(part, ..)| part == "subject").unwrap();
    let body = content.iter().find(|(part, ..)| part == "body").unwrap();
    assert_eq!(subject.2, None, "too short to tell");
    assert_eq!(body.2.as_deref(), Some("eng"));
}

#[tokio::test]
async fn chars_counts_characters_not_bytes() {
    // What a reader means by "how long is this".
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("héllo".to_owned()),
            ..Default::default()
        })
        .await;

    fx.extract(message_id).await;

    let content = fx.content(message_id);
    let subject = content.iter().find(|(part, ..)| part == "subject").unwrap();
    assert_eq!(subject.3, 5, "five characters, six bytes");
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn normalization_is_stable_across_meaningless_reformatting() {
    assert_eq!(normalize("  hello   world  "), "hello world");
    assert_eq!(normalize("hello\r\nworld"), "hello world");
    assert_eq!(normalize("hello\n\n\n\tworld"), "hello world");
    assert_eq!(normalize(""), "");
    assert_eq!(normalize("   \t\n  "), "");
    assert_eq!(
        normalize("zero\u{200B}width"),
        "zerowidth",
        "invisible characters would make two identical-looking bodies hash \
         differently"
    );
    assert_eq!(normalize("bell\u{7}char"), "bellchar");
    assert_eq!(
        normalize("naïve café"),
        "naïve café",
        "text survives intact"
    );
}

#[test]
fn part_keys_round_trip() {
    for part in [
        Part::Subject,
        Part::Headers,
        Part::Body,
        Part::Note,
        Part::Summary,
        Part::Attachment("2.1".to_owned()),
    ] {
        assert_eq!(Part::parse(&part.as_key()).unwrap(), part);
    }
    assert_eq!(
        Part::parse("from-the-future").unwrap_err().reason(),
        ErrorReason::Internal
    );
}

#[test]
fn part_keys_are_stable() {
    // Stored in index_content and matched by the removal sweep; changing one
    // silently orphans every row that used it.
    assert_eq!(Part::Subject.as_key(), "subject");
    assert_eq!(Part::Headers.as_key(), "headers");
    assert_eq!(Part::Body.as_key(), "body");
    assert_eq!(Part::Note.as_key(), "note");
    assert_eq!(Part::Summary.as_key(), "summary");
    assert_eq!(Part::Attachment("3".to_owned()).as_key(), "attachment:3");
}

#[test]
fn the_message_hash_covers_which_parts_exist_not_just_their_text() {
    // A part disappearing changes what the later stages should index, even when
    // every surviving part is byte-identical.
    let part = |key: Part, text: &str| (key.as_key(), hash(text));

    let both = message_hash(&[part(Part::Subject, "subject"), part(Part::Body, "body")]);
    let one = message_hash(&[part(Part::Subject, "subject")]);
    assert_ne!(both, one);

    // It is a hash of a *set*: order does not matter.
    let reversed = message_hash(&[part(Part::Body, "body"), part(Part::Subject, "subject")]);
    assert_eq!(both, reversed);

    // But *which* part carried the text does. Identical text under a different
    // key is a different document — the same words in a subject and in a body
    // rank differently — so the key has to be inside the hash, not merely used
    // to order it.
    assert_ne!(
        message_hash(&[part(Part::Body, "same text")]),
        message_hash(&[part(Part::Note, "same text")]),
    );

    // And swapping which part holds which text changes it too.
    let swapped = message_hash(&[part(Part::Subject, "body"), part(Part::Body, "subject")]);
    assert_ne!(both, swapped);
}

#[test]
fn an_empty_extraction_still_hashes_to_something_stable() {
    assert_eq!(message_hash(&[]), message_hash(&[]));
    assert!(!message_hash(&[]).is_empty());
}

// ---------------------------------------------------------------------------
// The parts other subsystems own, and the ones this stage would otherwise wipe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_attachment_row_survives_re_extraction() {
    // Attachment text is produced by the attachment pipeline, not here. A sweep
    // that deleted everything this stage did not itself produce would wipe
    // minutes of OCR work on every routine re-extract — silently, because the
    // message hash would not move and nothing downstream would re-run.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Has a scan attached".to_owned()),
            ..Default::default()
        })
        .await;
    fx.extract(message_id).await;
    fx.db
        .write(move |c| {
            c.execute(
                "INSERT INTO index_content
                     (message_id, part, text, chars, content_hash, extractor)
                 VALUES (?1, 'attachment:2', 'the scanned contract', 20, X'0102', 'ocr')",
                [message_id],
            )
        })
        .await
        .unwrap();

    let report = fx.extract(message_id).await;

    assert!(report.removed.is_empty(), "removed: {:?}", report.removed);
    assert_eq!(
        fx.text_of(message_id, "attachment:2").as_deref(),
        Some("the scanned contract")
    );
}

#[tokio::test]
async fn a_note_appearing_re_queues_the_stages_that_would_index_it() {
    // The follow-on hash has to cover every *stored* part, not just the ones
    // this stage produced. A note that left the hash byte-identical would be
    // deduped away and never indexed at all.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Plain message".to_owned()),
            ..Default::default()
        })
        .await;
    let first = fx.extract(message_id).await;
    for lease in fx.queue.lease("w", 10).await.unwrap() {
        fx.queue.complete(&lease, None).await.unwrap();
    }

    fx.db
        .write(move |c| {
            c.execute(
                "INSERT INTO index_content
                     (message_id, part, text, chars, content_hash, extractor)
                 VALUES (?1, 'note', 'call them back', 14, X'ABCD', 'user')",
                [message_id],
            )
        })
        .await
        .unwrap();
    let second = fx.extract(message_id).await;

    assert_ne!(
        second.content_hash, first.content_hash,
        "the note is searchable, so the stages that index it must re-run"
    );
    assert_eq!(second.follow_on, 3);
}

#[tokio::test]
async fn html_that_defeats_the_renderer_is_indexed_as_empty_not_a_panic() {
    // This path is reached exactly when the parse stage's stripper already
    // failed — its fallback is empty text, and empty text is what sends the
    // body here — so the pathological input is adversarially selected for.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Nested".to_owned()),
            // Properly closed, because that is what html2text actually nests,
            // and deep enough that the indentation exceeds the render width.
            body_html: Some(format!(
                "{}hello{}",
                "<blockquote>".repeat(150),
                "</blockquote>".repeat(150)
            )),
            ..Default::default()
        })
        .await;

    let report = fx.extract(message_id).await;

    assert!(report.written.contains(&Part::Subject));
    assert!(
        fx.text_of(message_id, "body")
            .is_none_or(|body| body.is_empty()),
        "unrenderable HTML indexes as nothing rather than taking the process down"
    );
}

#[tokio::test]
async fn an_oversized_html_body_is_skipped_rather_than_rendered() {
    // html2text is quadratic in nesting depth and runs on a blocking thread
    // that cannot be aborted. One crafted message would otherwise pin a pool
    // thread for minutes, five times over, because the queue cannot tell slow
    // from broken.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            body_html: Some("<p>x</p>".repeat(MAX_HTML_BYTES / 8 + 1)),
            ..Default::default()
        })
        .await;

    let report = fx.extract(message_id).await;

    assert!(report.written.is_empty());
    assert!(fx.text_of(message_id, "body").is_none());
}

#[tokio::test]
async fn a_new_extractor_version_rewrites_rows_whose_text_did_not_change() {
    // The hash covers the text alone, so an unchanged body whose extractor
    // moved on still needs rewriting — otherwise bumping EXTRACTOR leaves every
    // untouched row claiming the old one, which defeats the point of storing
    // it.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            subject: Some("Unchanged text".to_owned()),
            ..Default::default()
        })
        .await;
    fx.extract(message_id).await;
    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE index_content SET extractor = 'rmail/text@0' WHERE message_id = ?1",
                [message_id],
            )
        })
        .await
        .unwrap();

    let report = fx.extract(message_id).await;

    assert_eq!(report.written, vec![Part::Subject]);
    let extractor: String = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT extractor FROM index_content WHERE message_id = ?1 AND part = 'subject'",
                [message_id],
                |row| row.get(0),
            )
        })
        .unwrap();
    assert_eq!(extractor, EXTRACTOR);
}

#[tokio::test]
async fn the_stored_row_carries_a_hash_a_mime_type_and_an_extractor() {
    // The acceptance criterion names all three; nothing else reads them back.
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            body_text: Some("A plain text body.".to_owned()),
            ..Default::default()
        })
        .await;

    fx.extract(message_id).await;

    let (hash, mime, extractor): (Vec<u8>, Option<String>, Option<String>) = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT content_hash, mime, extractor FROM index_content
                 WHERE message_id = ?1 AND part = 'body'",
                [message_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
        })
        .unwrap();
    assert_eq!(hash.len(), 32, "a SHA-256 digest");
    assert_eq!(mime.as_deref(), Some("text/plain"));
    assert_eq!(extractor.as_deref(), Some(EXTRACTOR));
}

#[tokio::test]
async fn an_html_only_body_is_labelled_as_html() {
    let fx = Fixture::open().await;
    let message_id = fx
        .store(repo::NewMessage {
            body_html: Some("<p>Only markup here.</p>".to_owned()),
            ..Default::default()
        })
        .await;

    fx.extract(message_id).await;

    let mime: Option<String> = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT mime FROM index_content WHERE message_id = ?1 AND part = 'body'",
                [message_id],
                |row| row.get(0),
            )
        })
        .unwrap();
    assert_eq!(mime.as_deref(), Some("text/html"));
}

#[tokio::test]
async fn the_priority_reaches_the_queued_follow_on_jobs() {
    // A message the user just opened should not have its lexical and semantic
    // work land behind the whole backfill.
    let fx = Fixture::open().await;
    let urgent = fx
        .store(repo::NewMessage {
            subject: Some("Just opened".to_owned()),
            ..Default::default()
        })
        .await;
    let background = fx
        .store(repo::NewMessage {
            subject: Some("From the archive".to_owned()),
            ..Default::default()
        })
        .await;

    extract_message(
        &fx.db,
        &fx.queue,
        background,
        crate::index::PRIORITY_BACKFILL,
    )
    .await
    .unwrap();
    extract_message(&fx.db, &fx.queue, urgent, crate::index::PRIORITY_RECENT)
        .await
        .unwrap();

    let leased = fx.queue.lease("w", 3).await.unwrap();
    assert!(
        leased.iter().all(|lease| lease.message_id == urgent),
        "the urgent message's stages come first"
    );
}

#[test]
fn normalization_folds_unicode_and_drops_invisibles() {
    use unicode_normalization::UnicodeNormalization;

    // Decomposed and precomposed forms render identically. Hashing them
    // differently would re-embed the message and then fail to match a query
    // typed the other way.
    let decomposed: String = "cafe\u{301}".nfd().collect();
    assert_eq!(normalize(&decomposed), normalize("café"));

    // A soft hyphen inside a word is invisible and would split it into a token
    // nobody can search for.
    assert_eq!(normalize("inter\u{00AD}national"), "international");
    // Bidi controls are the Trojan-Source display-spoofing vector, and a search
    // snippet is a rendering surface.
    assert_eq!(normalize("safe\u{202E}txt"), "safetxt");
    assert_eq!(normalize("word\u{2060}joiner"), "wordjoiner");

    // But a zero-width non-joiner separates words in Persian, Arabic and Hindi.
    // Deleting it would weld them into a token nobody will type.
    assert_eq!(normalize("می\u{200C}روم"), "می روم");
}
