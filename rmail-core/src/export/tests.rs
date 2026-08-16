//! End-to-end export tests: every format round-trips, the raw RFC822 comes
//! back byte-identical, `--with-ai` attaches what the AI passes stored, and
//! the error paths fail the way the docs claim.
//!
//! The mbox reader below is written from the mboxrd rule rather than by
//! inverting `export::mbox`'s code, so a bug in the writer cannot cancel out
//! against a matching bug in the reader.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::repo::{self, NewAccount, NewAttachment, NewMailbox, NewMessage, NewThread};
use crate::storage::Database;
use crate::ErrorReason;

use super::write::{DestinationWriter, WriteError};
use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    db: Database,
    db_path: PathBuf,
    dir: PathBuf,
    account_id: i64,
    mailbox_id: i64,
    next_uid: i64,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let db_path = std::env::temp_dir().join(format!("rmail-export-{pid}-{n}.db"));
        let dir = std::env::temp_dir().join(format!("rmail-export-{pid}-{n}.out"));
        let db = Database::open(&db_path).unwrap();

        let (account_id, mailbox_id) = db
            .write(|conn| {
                let account_id = repo::insert_account(
                    conn,
                    &NewAccount {
                        name: "primary".into(),
                        imap_server: Some("imap.example.com".into()),
                        imap_port: Some(993),
                        username: Some("ada".into()),
                        smtp_server: None,
                        smtp_port: None,
                        secret_kind: None,
                        secret_ref: None,
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    conn,
                    &NewMailbox {
                        account_id,
                        name: "INBOX".into(),
                        uidvalidity: Some(1),
                        uidnext: Some(1),
                        highestmodseq: None,
                        attributes: None,
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .unwrap();

        Self {
            db,
            db_path,
            dir,
            account_id,
            mailbox_id,
            next_uid: 1,
        }
    }

    /// Insert a message with the exact raw bytes given.
    async fn message(&mut self, subject: &str, from: &str, raw: &[u8]) -> i64 {
        self.message_with(subject, from, Some(raw.to_vec()), None)
            .await
    }

    async fn message_with(
        &mut self,
        subject: &str,
        from: &str,
        raw: Option<Vec<u8>>,
        thread_id: Option<i64>,
    ) -> i64 {
        let uid = self.next_uid;
        self.next_uid += 1;
        let new = NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            message_id: Some(format!("<msg-{uid}@example.com>")),
            thread_id,
            in_reply_to: None,
            references_hdr: None,
            subject: Some(subject.to_owned()),
            from_addr: Some(from.to_owned()),
            from_name: Some("Ada".to_owned()),
            to_addrs: Some("bob@example.com, carol@example.com".to_owned()),
            cc_addrs: None,
            date: Some(1_700_000_000 + uid),
            internaldate: Some(1_700_000_000 + uid),
            size: raw.as_ref().map(|r| r.len() as i64),
            raw,
            body_text: Some("body".to_owned()),
            body_html: None,
            has_attachments: false,
        };
        self.db
            .write(move |conn| repo::insert_message(conn, &new))
            .await
            .unwrap()
    }

    async fn set_flags(&self, message_id: i64, flags: &[&str]) {
        let flags: Vec<String> = flags.iter().map(|f| (*f).to_owned()).collect();
        self.db
            .write(move |conn| repo::replace_flags(conn, message_id, &flags))
            .await
            .unwrap();
    }

    async fn thread(&self, subject: &str) -> i64 {
        let account_id = self.account_id;
        let subject = subject.to_owned();
        self.db
            .write(move |conn| {
                repo::insert_thread(
                    conn,
                    &NewThread {
                        account_id,
                        subject_norm: Some(subject),
                        root_message_id: None,
                        first_message_at: None,
                        last_message_at: None,
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn index(&self, message_id: i64, text: &str) {
        let text = text.to_owned();
        self.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO fts_messages \
                     (rowid, subject, sender, recipients, body, attachments, notes, summary) \
                     VALUES (?1, '', '', '', ?2, '', '', '')",
                    rusqlite::params![message_id, text],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    fn exporter(&self) -> Exporter {
        Exporter::new(self.db.clone())
    }

    async fn run(&self, selection: &Selection, options: &ExportOptions) -> Vec<Chunk> {
        let mut sink: Vec<Chunk> = Vec::new();
        self.exporter()
            .export(selection, options, &CancellationToken::new(), &mut sink)
            .await
            .unwrap();
        sink
    }

    /// Export straight to disk under `self.dir`, returning the destination.
    async fn export_to_disk(&self, selection: &Selection, options: &ExportOptions) -> PathBuf {
        let chunks = self.run(selection, options).await;
        let destination = if options.format.is_single_stream() {
            self.dir.join(format!("archive.{}", options.format))
        } else {
            self.dir.join("archive")
        };
        let mut writer = DestinationWriter::create(options.format, &destination).unwrap();
        for chunk in &chunks {
            writer.apply(chunk).unwrap();
        }
        writer.finish().unwrap();
        destination
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Concatenate every chunk's bytes — what a single-stream consumer produces.
fn concat(chunks: &[Chunk]) -> Vec<u8> {
    chunks.iter().flat_map(|c| c.data.clone()).collect()
}

// ---------------------------------------------------------------------------
// An mbox reader, written from the spec
// ---------------------------------------------------------------------------

/// Split an mbox into its messages' original bytes.
///
/// Deliberately an independent implementation of the inverse of
/// [`super::mbox::frame`]: split on lines beginning `From ` at column zero,
/// drop the separator line, strip exactly one trailing `\n`, and remove one
/// leading `>` from every line matching `^>+From `.
fn read_mbox(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut messages: Vec<Vec<u8>> = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    let mut offset = 0;
    while offset < bytes.len() {
        let end = bytes[offset..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |idx| offset + idx + 1);
        let line = &bytes[offset..end];
        offset = end;

        let depth = line.iter().position(|&b| b != b'>').unwrap_or(line.len());
        if depth == 0 && line.starts_with(b"From ") {
            if let Some(previous) = current.take() {
                messages.push(strip_one_newline(previous));
            }
            current = Some(Vec::new());
            continue;
        }
        let Some(body) = current.as_mut() else {
            panic!("mbox content before the first From_ line");
        };
        if depth > 0 && line[depth..].starts_with(b"From ") {
            body.extend_from_slice(&line[1..]);
        } else {
            body.extend_from_slice(line);
        }
    }
    if let Some(previous) = current.take() {
        messages.push(strip_one_newline(previous));
    }
    messages
}

fn strip_one_newline(mut body: Vec<u8>) -> Vec<u8> {
    if body.last() == Some(&b'\n') {
        body.pop();
    }
    body
}

/// A realistic message, CRLF throughout, containing a line that mbox framing
/// must escape.
fn sample_raw(n: usize) -> Vec<u8> {
    format!(
        "Message-ID: <msg-{n}@example.com>\r\n\
         From: Ada <ada@example.com>\r\n\
         To: bob@example.com\r\n\
         Subject: Quarterly report {n}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         From now on we ship on Fridays.\r\n\
         >From the desk of Ada\r\n\
         Regards\r\n"
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mbox_round_trips_every_message_byte_for_byte() {
    let mut fx = Fixture::open().await;
    let raws: Vec<Vec<u8>> = (0..3).map(sample_raw).collect();
    for (n, raw) in raws.iter().enumerate() {
        fx.message(&format!("Quarterly report {n}"), "ada@example.com", raw)
            .await;
    }

    let chunks = fx
        .run(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Mbox),
        )
        .await;
    let archive = concat(&chunks);

    assert_eq!(read_mbox(&archive), raws);
    assert!(
        archive.starts_with(b"From ada@example.com "),
        "the archive must open with a From_ separator"
    );
}

#[tokio::test]
async fn mbox_escaping_survives_a_body_that_looks_like_a_separator() {
    let mut fx = Fixture::open().await;
    // Every quoting depth, plus a bare `From ` at the very start of the body.
    let raw = b"Subject: x\r\n\r\nFrom nowhere\r\n>From nowhere\r\n>>From nowhere\r\n".to_vec();
    fx.message("x", "ada@example.com", &raw).await;

    let archive = concat(
        &fx.run(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Mbox),
        )
        .await,
    );

    assert_eq!(read_mbox(&archive), vec![raw]);
    // Exactly one unescaped separator line in the whole archive.
    let separators = archive
        .split(|&b| b == b'\n')
        .filter(|line| line.starts_with(b"From "))
        .count();
    assert_eq!(separators, 1, "escaping let a body line act as a separator");
}

#[tokio::test]
async fn eml_round_trips_and_writes_one_file_per_message() {
    let mut fx = Fixture::open().await;
    let raws: Vec<Vec<u8>> = (0..2).map(sample_raw).collect();
    for (n, raw) in raws.iter().enumerate() {
        fx.message(&format!("Quarterly report {n}"), "ada@example.com", raw)
            .await;
    }

    let dir = fx
        .export_to_disk(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Eml),
        )
        .await;

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    assert_eq!(files.len(), 2);
    for (file, raw) in files.iter().zip(&raws) {
        assert_eq!(&std::fs::read(file).unwrap(), raw);
    }
    // The slug is a courtesy; the id prefix is what makes it unique.
    assert!(
        files[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".eml"),
        "{files:?}"
    );
    assert!(files[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .contains("quarterly-report"));
}

#[tokio::test]
async fn maildir_round_trips_and_encodes_flags_in_the_filename() {
    let mut fx = Fixture::open().await;
    let raw = sample_raw(1);
    let id = fx
        .message("Quarterly report 1", "ada@example.com", &raw)
        .await;
    fx.set_flags(id, &["\\Seen", "\\Answered"]).await;

    let dir = fx
        .export_to_disk(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Maildir),
        )
        .await;

    for sub in write::MAILDIR_DIRS {
        assert!(dir.join(sub).is_dir(), "missing maildir subdirectory {sub}");
    }
    let files: Vec<PathBuf> = std::fs::read_dir(dir.join("cur"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1);
    let name = files[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with(":2,RS"), "flags not encoded: {name}");
    assert_eq!(std::fs::read(&files[0]).unwrap(), raw);
}

#[tokio::test]
async fn json_round_trips_the_raw_bytes_through_base64() {
    let mut fx = Fixture::open().await;
    let raws: Vec<Vec<u8>> = (0..2).map(sample_raw).collect();
    for (n, raw) in raws.iter().enumerate() {
        let id = fx
            .message(&format!("Quarterly report {n}"), "ada@example.com", raw)
            .await;
        fx.set_flags(id, &["\\Seen"]).await;
    }

    let archive = concat(
        &fx.run(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Json),
        )
        .await,
    );

    let document: Value = serde_json::from_slice(&archive).expect("a single valid JSON document");
    assert_eq!(document["version"], json::SCHEMA_VERSION);
    let messages = document["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);

    for (record, raw) in messages.iter().zip(&raws) {
        let encoded = record["raw_rfc822_base64"].as_str().unwrap();
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded).unwrap();
        assert_eq!(&decoded, raw, "raw RFC822 did not survive the round trip");
        assert_eq!(record["mailbox"], "INBOX");
        assert_eq!(record["flags"], serde_json::json!(["\\Seen"]));
        assert_eq!(record["from"]["address"], "ada@example.com");
        assert_eq!(
            record["to"],
            serde_json::json!(["bob@example.com", "carol@example.com"])
        );
        // Absent, not null: this export did not ask for AI.
        assert!(record.get("ai").is_none());
    }
}

#[tokio::test]
async fn an_empty_selection_still_produces_a_well_formed_json_document() {
    let fx = Fixture::open().await;
    let archive = concat(
        &fx.run(
            &Selection::Query("from:nobody@example.com".into()),
            &ExportOptions::new(Format::Json),
        )
        .await,
    );
    let document: Value = serde_json::from_slice(&archive).unwrap();
    assert_eq!(document["messages"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn json_attachment_metadata_is_carried_but_not_its_bytes() {
    let mut fx = Fixture::open().await;
    let raw = sample_raw(1);
    let id = fx.message("with attachment", "ada@example.com", &raw).await;
    fx.db
        .write(move |conn| {
            repo::insert_attachment(
                conn,
                &NewAttachment {
                    message_id: id,
                    part_id: Some("2".into()),
                    filename: Some("invoice.pdf".into()),
                    content_type: Some("application/pdf".into()),
                    size: Some(4096),
                    content_id: None,
                    is_inline: false,
                },
            )
        })
        .await
        .unwrap();

    let archive = concat(
        &fx.run(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Json),
        )
        .await,
    );
    let document: Value = serde_json::from_slice(&archive).unwrap();
    let attachments = document["messages"][0]["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["filename"], "invoice.pdf");
    assert_eq!(attachments[0]["size"], 4096);
    assert!(attachments[0].get("bytes").is_none());
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_operator_query_selects_only_matching_messages() {
    let mut fx = Fixture::open().await;
    fx.message("keep", "alice@example.com", &sample_raw(1))
        .await;
    fx.message("drop", "mallory@example.com", &sample_raw(2))
        .await;

    let archive = concat(
        &fx.run(
            &Selection::Query("from:alice".into()),
            &ExportOptions::new(Format::Mbox),
        )
        .await,
    );
    let messages = read_mbox(&archive);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0], sample_raw(1));
}

#[tokio::test]
async fn free_text_selects_through_the_lexical_index() {
    let mut fx = Fixture::open().await;
    let wanted = fx.message("a", "ada@example.com", &sample_raw(1)).await;
    let other = fx.message("b", "ada@example.com", &sample_raw(2)).await;
    fx.index(wanted, "quarterly revenue figures").await;
    fx.index(other, "lunch plans").await;

    let archive = concat(
        &fx.run(
            &Selection::Query("revenue".into()),
            &ExportOptions::new(Format::Mbox),
        )
        .await,
    );
    assert_eq!(read_mbox(&archive), vec![sample_raw(1)]);
}

#[tokio::test]
async fn a_thread_exports_in_conversation_order() {
    let mut fx = Fixture::open().await;
    let thread_id = fx.thread("Office move").await;
    // Inserted newest-first so id order and date order disagree.
    let second = fx
        .message_with(
            "re: move",
            "ada@example.com",
            Some(sample_raw(2)),
            Some(thread_id),
        )
        .await;
    let first = fx
        .message_with(
            "move",
            "ada@example.com",
            Some(sample_raw(1)),
            Some(thread_id),
        )
        .await;
    // Make the second-inserted message the older one.
    fx.db
        .write(move |conn| {
            conn.execute("UPDATE messages SET date = 1 WHERE id = ?1", [first])?;
            conn.execute("UPDATE messages SET date = 2 WHERE id = ?1", [second])?;
            Ok(())
        })
        .await
        .unwrap();

    let chunks = fx
        .run(
            &Selection::Thread(thread_id),
            &ExportOptions::new(Format::Mbox),
        )
        .await;
    let ids: Vec<i64> = chunks
        .iter()
        .filter(|c| c.start_of_message)
        .filter_map(|c| c.message_id)
        .collect();
    assert_eq!(ids, vec![first, second]);
}

/// The defect this guards: search's compilers are fail-open, so a query whose
/// free text or operators do not survive compilation silently becomes "no
/// constraint" — which for an export means writing the entire mailbox to a
/// file named after Alice's invoices. Every shape below reached
/// `SELECT id FROM messages` with no WHERE clause before this check existed.
#[tokio::test]
async fn a_query_whose_constraints_cannot_be_enforced_is_refused_not_widened() {
    let mut fx = Fixture::open().await;
    for n in 0..3 {
        fx.message(&format!("m{n}"), "ada@example.com", &sample_raw(n))
            .await;
    }

    for query in [
        // Every term forced semantic: export runs no dense retriever.
        "~invoice",
        // Half the query enforceable, half not — the quiet one.
        "from:ada ~invoice",
        // No character the tokenizer produces a token from.
        "🎉",
        // A date expression that does not parse.
        "after:lasst-week",
        // An inverted range resolves to nothing usable.
        "date:2025-08..2025-06",
    ] {
        let mut sink: Vec<Chunk> = Vec::new();
        let result = fx
            .exporter()
            .export(
                &Selection::Query(query.into()),
                &ExportOptions::new(Format::Mbox),
                &CancellationToken::new(),
                &mut sink,
            )
            .await;
        match result {
            Ok(summary) => panic!(
                "{query:?} exported {} message(s) instead of being refused",
                summary.messages
            ),
            Err(error) => assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{query:?}"),
        }
        assert!(
            sink.is_empty(),
            "{query:?} wrote bytes before being refused"
        );
    }
}

#[tokio::test]
async fn an_empty_query_still_means_archive_everything() {
    let mut fx = Fixture::open().await;
    for n in 0..3 {
        fx.message(&format!("m{n}"), "ada@example.com", &sample_raw(n))
            .await;
    }
    let chunks = fx
        .run(
            &Selection::Query("   ".into()),
            &ExportOptions::new(Format::Mbox),
        )
        .await;
    assert_eq!(read_mbox(&concat(&chunks)).len(), 3);
}

#[tokio::test]
async fn a_limit_stops_the_export_early() {
    let mut fx = Fixture::open().await;
    for n in 0..5 {
        fx.message(&format!("m{n}"), "ada@example.com", &sample_raw(n))
            .await;
    }

    let mut sink: Vec<Chunk> = Vec::new();
    let summary = fx
        .exporter()
        .export(
            &Selection::Query(String::new()),
            &ExportOptions {
                format: Format::Mbox,
                with_ai: false,
                limit: Some(2),
            },
            &CancellationToken::new(),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(summary.messages, 2);
    assert!(summary.complete);
    assert_eq!(read_mbox(&concat(&sink)).len(), 2);
}

/// The limit counts what reached the archive, not what the selection matched:
/// a row with no stored raw is skipped by a byte format and must not spend the
/// budget, or `--limit 2` quietly produces one.
#[tokio::test]
async fn a_limit_counts_exported_messages_not_selected_rows() {
    let mut fx = Fixture::open().await;
    fx.message_with("no raw", "ada@example.com", None, None)
        .await;
    for n in 0..3 {
        fx.message(&format!("m{n}"), "ada@example.com", &sample_raw(n))
            .await;
    }

    let mut sink: Vec<Chunk> = Vec::new();
    let summary = fx
        .exporter()
        .export(
            &Selection::Query(String::new()),
            &ExportOptions {
                format: Format::Mbox,
                with_ai: false,
                limit: Some(2),
            },
            &CancellationToken::new(),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(summary.messages, 2);
    assert_eq!(summary.skipped_without_raw, 1);
    assert_eq!(read_mbox(&concat(&sink)).len(), 2);
}

#[tokio::test]
async fn an_export_spanning_several_keyset_pages_yields_every_message_once() {
    let mut fx = Fixture::open().await;
    let total = PAGE_SIZE + 7;
    for n in 0..total {
        fx.message(&format!("m{n}"), "ada@example.com", &sample_raw(n))
            .await;
    }

    let chunks = fx
        .run(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Eml),
        )
        .await;
    let mut ids: Vec<i64> = chunks
        .iter()
        .filter(|c| c.start_of_message)
        .filter_map(|c| c.message_id)
        .collect();
    let before = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(before, total, "wrong message count across page boundaries");
    assert_eq!(ids.len(), total, "a message was exported twice");
}

#[tokio::test]
async fn a_message_larger_than_one_chunk_is_split_and_reassembles() {
    let mut fx = Fixture::open().await;
    let mut raw = b"Subject: big\r\n\r\n".to_vec();
    raw.extend(std::iter::repeat_n(b'x', CHUNK_BYTES * 2 + 11));
    raw.extend_from_slice(b"\r\n");
    fx.message("big", "ada@example.com", &raw).await;

    let chunks = fx
        .run(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Eml),
        )
        .await;
    assert!(chunks.len() > 2, "a large message should span chunks");
    assert_eq!(chunks.iter().filter(|c| c.start_of_message).count(), 1);
    assert!(chunks.iter().all(|c| c.data.len() <= CHUNK_BYTES));
    assert_eq!(concat(&chunks), raw);
}

// ---------------------------------------------------------------------------
// --with-ai
// ---------------------------------------------------------------------------

#[tokio::test]
async fn with_ai_attaches_stored_summaries_and_tags() {
    let mut fx = Fixture::open().await;
    let id = fx
        .message("Invoice", "ada@example.com", &sample_raw(1))
        .await;
    let account_id = fx.account_id;
    fx.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO ai_summaries (
                     message_id, account_id, model, pass, schema_version, tl_dr,
                     suggested_tags, category, needs_reply, created_at
                 ) VALUES (?1, ?2, 'claude-test', 'triage', 1, 'Invoice due Friday',
                           '[\"finance\"]', 'invoice', 1, 42)",
                rusqlite::params![id, account_id],
            )?;
            let tag_id = {
                conn.execute(
                    "INSERT INTO tags (account_id, name) VALUES (?1, 'finance')",
                    [account_id],
                )?;
                conn.last_insert_rowid()
            };
            conn.execute(
                "INSERT INTO message_tags (tag_id, message_id, source, state) \
                 VALUES (?1, ?2, 'ai', 'applied')",
                rusqlite::params![tag_id, id],
            )?;
            // A pending suggestion must not reach the archive.
            conn.execute(
                "INSERT INTO tags (account_id, name) VALUES (?1, 'maybe')",
                [account_id],
            )?;
            let pending = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO message_tags (tag_id, message_id, source, state) \
                 VALUES (?1, ?2, 'ai', 'pending')",
                rusqlite::params![pending, id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let archive = concat(
        &fx.run(
            &Selection::Query(String::new()),
            &ExportOptions {
                format: Format::Json,
                with_ai: true,
                limit: None,
            },
        )
        .await,
    );
    let document: Value = serde_json::from_slice(&archive).unwrap();
    let ai = &document["messages"][0]["ai"];
    assert_eq!(ai["tags"], serde_json::json!(["finance"]));
    let summaries = ai["summaries"].as_array().unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0]["pass"], "triage");
    assert_eq!(summaries[0]["tl_dr"], "Invoice due Friday");
    assert_eq!(summaries[0]["needs_reply"], true);
    // Stored as a JSON string in SQLite; re-parsed into a real array here.
    assert_eq!(
        summaries[0]["suggested_tags"],
        serde_json::json!(["finance"])
    );
}

#[tokio::test]
async fn with_ai_on_a_message_with_no_artifacts_yields_empty_collections() {
    let mut fx = Fixture::open().await;
    fx.message("plain", "ada@example.com", &sample_raw(1)).await;

    let archive = concat(
        &fx.run(
            &Selection::Query(String::new()),
            &ExportOptions {
                format: Format::Json,
                with_ai: true,
                limit: None,
            },
        )
        .await,
    );
    let document: Value = serde_json::from_slice(&archive).unwrap();
    assert_eq!(
        document["messages"][0]["ai"]["summaries"],
        serde_json::json!([])
    );
    assert_eq!(document["messages"][0]["ai"]["tags"], serde_json::json!([]));
}

#[tokio::test]
async fn with_ai_is_refused_for_the_byte_formats() {
    let fx = Fixture::open().await;
    for format in [Format::Mbox, Format::Maildir, Format::Eml] {
        let mut sink: Vec<Chunk> = Vec::new();
        let error = fx
            .exporter()
            .export(
                &Selection::Query(String::new()),
                &ExportOptions {
                    format,
                    with_ai: true,
                    limit: None,
                },
                &CancellationToken::new(),
                &mut sink,
            )
            .await
            .expect_err("--with-ai has nowhere to go in a byte format");
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{format}");
        assert!(sink.is_empty(), "the export must not have started");
    }
}

// ---------------------------------------------------------------------------
// Error and edge paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exporting_a_thread_that_does_not_exist_is_not_found() {
    let fx = Fixture::open().await;
    let mut sink: Vec<Chunk> = Vec::new();
    let error = fx
        .exporter()
        .export(
            &Selection::Thread(4242),
            &ExportOptions::new(Format::Mbox),
            &CancellationToken::new(),
            &mut sink,
        )
        .await
        .expect_err("a missing thread must not export as an empty archive");
    assert_eq!(error.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn a_negative_limit_is_invalid_argument() {
    let fx = Fixture::open().await;
    let mut sink: Vec<Chunk> = Vec::new();
    let error = fx
        .exporter()
        .export(
            &Selection::Query(String::new()),
            &ExportOptions {
                format: Format::Mbox,
                with_ai: false,
                limit: Some(-1),
            },
            &CancellationToken::new(),
            &mut sink,
        )
        .await
        .expect_err("a negative limit is not a selection");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn a_cancelled_export_fails_rather_than_reporting_a_short_archive() {
    let mut fx = Fixture::open().await;
    for n in 0..3 {
        fx.message(&format!("m{n}"), "ada@example.com", &sample_raw(n))
            .await;
    }
    let cancel = CancellationToken::new();
    cancel.cancel();

    let mut sink: Vec<Chunk> = Vec::new();
    let error = fx
        .exporter()
        .export(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Mbox),
            &cancel,
            &mut sink,
        )
        .await
        .expect_err("a cancelled export must not look like a complete one");
    assert_eq!(error.reason(), ErrorReason::Cancelled);
}

#[tokio::test]
async fn a_message_with_no_stored_raw_is_counted_not_fabricated() {
    let mut fx = Fixture::open().await;
    fx.message("has raw", "ada@example.com", &sample_raw(1))
        .await;
    fx.message_with("no raw", "ada@example.com", None, None)
        .await;

    let mut sink: Vec<Chunk> = Vec::new();
    let summary = fx
        .exporter()
        .export(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Mbox),
            &CancellationToken::new(),
            &mut sink,
        )
        .await
        .unwrap();
    assert_eq!(summary.messages, 1);
    assert_eq!(summary.skipped_without_raw, 1);
    assert_eq!(read_mbox(&concat(&sink)).len(), 1);
}

#[tokio::test]
async fn json_reports_a_missing_raw_as_null_rather_than_dropping_the_record() {
    let mut fx = Fixture::open().await;
    fx.message_with("no raw", "ada@example.com", None, None)
        .await;

    let archive = concat(
        &fx.run(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Json),
        )
        .await,
    );
    let document: Value = serde_json::from_slice(&archive).unwrap();
    let messages = document["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert!(messages[0]["raw_rfc822_base64"].is_null());
}

#[tokio::test]
async fn a_malformed_free_text_query_is_invalid_argument_not_internal() {
    let mut fx = Fixture::open().await;
    fx.message("m", "ada@example.com", &sample_raw(1)).await;

    let mut sink: Vec<Chunk> = Vec::new();
    let error = fx
        .exporter()
        .export(
            // A NUL inside a quoted phrase survives quoting and FTS5 rejects
            // the expression — the one way user text still reaches SQLite as
            // a syntax error (see `retrieve::lexical`'s own docs).
            &Selection::Query("\"bad\u{0}phrase\"".into()),
            &ExportOptions::new(Format::Mbox),
            &CancellationToken::new(),
            &mut sink,
        )
        .await
        .expect_err("a malformed FTS expression must be the caller's fault");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

/// A sink that reports itself closed after `accept_before` chunks, and can
/// fire a cancellation token when it does — the two ways an export stops
/// early, driven deterministically.
struct StoppingSink {
    accepted: Vec<Chunk>,
    accept_before: usize,
    cancel_at_stop: Option<CancellationToken>,
}

#[async_trait::async_trait]
impl ChunkSink for StoppingSink {
    async fn accept(&mut self, chunk: Chunk) -> Result<(), SinkClosed> {
        if self.accepted.len() >= self.accept_before {
            if let Some(cancel) = &self.cancel_at_stop {
                cancel.cancel();
            }
            return Err(SinkClosed);
        }
        self.accepted.push(chunk);
        Ok(())
    }
}

#[tokio::test]
async fn a_closed_sink_stops_the_export_without_claiming_it_is_complete() {
    let mut fx = Fixture::open().await;
    for n in 0..3 {
        fx.message(&format!("m{n}"), "ada@example.com", &sample_raw(n))
            .await;
    }

    let mut sink = StoppingSink {
        accepted: Vec::new(),
        accept_before: 1,
        cancel_at_stop: None,
    };
    let summary = fx
        .exporter()
        .export(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Mbox),
            &CancellationToken::new(),
            &mut sink,
        )
        .await
        .expect("a client hanging up is not a fault");
    assert_eq!(sink.accepted.len(), 1);
    assert!(
        !summary.complete,
        "a run the consumer cut short must not report itself complete"
    );
}

/// The contract the module docs claim: a cancellation that lands *mid*-export
/// is an error, not a short summary. The pre-cancelled case above never
/// reaches `emit` at all, so without this the check that matters is untested.
#[tokio::test]
async fn cancelling_mid_export_fails_rather_than_returning_a_short_summary() {
    let mut fx = Fixture::open().await;
    for n in 0..5 {
        fx.message(&format!("m{n}"), "ada@example.com", &sample_raw(n))
            .await;
    }

    let cancel = CancellationToken::new();
    // Accept two messages, then behave exactly as `rmaild`'s own sink does
    // when the daemon shuts down: fire the token and report closed.
    let mut sink = StoppingSink {
        accepted: Vec::new(),
        accept_before: 2,
        cancel_at_stop: Some(cancel.clone()),
    };
    let error = fx
        .exporter()
        .export(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Mbox),
            &cancel,
            &mut sink,
        )
        .await
        .expect_err("a cancelled export must not look like a finished one");
    assert_eq!(error.reason(), ErrorReason::Cancelled);
    assert_eq!(
        sink.accepted.len(),
        2,
        "the partial archive is still partial"
    );
}

/// A token that fires between pages — the other half of the same rule, on the
/// read side rather than the send side.
#[tokio::test]
async fn cancelling_between_pages_fails_the_export() {
    let mut fx = Fixture::open().await;
    for n in 0..3 {
        fx.message(&format!("m{n}"), "ada@example.com", &sample_raw(n))
            .await;
    }
    let cancel = CancellationToken::new();
    let mut sink = StoppingSink {
        accepted: Vec::new(),
        accept_before: usize::MAX,
        cancel_at_stop: None,
    };
    // Fire it after the export has begun but before it can finish: one
    // message's worth of chunks is enough to have passed `emit` at least once.
    let cancel_task = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            cancel.cancel();
        })
    };
    let result = fx
        .exporter()
        .export(
            &Selection::Query(String::new()),
            &ExportOptions::new(Format::Mbox),
            &cancel,
            &mut sink,
        )
        .await;
    let _ = cancel_task.await;
    // Either it finished before the token fired (a legitimate race on a
    // three-message fixture) or it failed — never a *short* success.
    match result {
        Ok(summary) => assert!(
            summary.complete,
            "an export that returned Ok must have been complete"
        ),
        Err(error) => assert_eq!(error.reason(), ErrorReason::Cancelled),
    }
}

// ---------------------------------------------------------------------------
// The writer half
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_writer_refuses_an_entry_path_that_escapes_the_destination() {
    let fx = Fixture::open().await;
    let dir = fx.dir.join("hostile");
    let mut writer = DestinationWriter::create(Format::Eml, &dir).unwrap();
    let error = writer
        .apply(&Chunk {
            path: Some("../../escaped.eml".into()),
            start_of_message: true,
            message_id: Some(1),
            data: b"x".to_vec(),
        })
        .expect_err("a traversing path must never be written");
    assert!(matches!(error, WriteError::UnsafePath { .. }), "{error}");
    assert!(!fx.dir.parent().unwrap().join("escaped.eml").exists());
}

#[tokio::test]
async fn the_writer_rejects_a_continuation_for_an_unopened_entry() {
    let fx = Fixture::open().await;
    let mut writer = DestinationWriter::create(Format::Eml, &fx.dir.join("out")).unwrap();
    let error = writer
        .apply(&Chunk {
            path: Some("1.eml".into()),
            start_of_message: false,
            message_id: Some(1),
            data: b"x".to_vec(),
        })
        .expect_err("a continuation without a start is a malformed stream");
    assert!(matches!(error, WriteError::Protocol(_)), "{error}");
}

#[tokio::test]
async fn a_single_stream_format_refuses_a_named_entry() {
    let fx = Fixture::open().await;
    let mut writer = DestinationWriter::create(Format::Mbox, &fx.dir.join("out.mbox")).unwrap();
    let error = writer
        .apply(&Chunk {
            path: Some("cur/1".into()),
            start_of_message: true,
            message_id: Some(1),
            data: b"x".to_vec(),
        })
        .expect_err("mbox is one document");
    assert!(matches!(error, WriteError::Protocol(_)), "{error}");
}

#[test]
fn a_per_file_format_cannot_be_written_to_a_stream() {
    let error = DestinationWriter::to_writer(Format::Maildir, Box::new(Vec::new()))
        .expect_err("a maildir needs a directory");
    assert!(matches!(error, WriteError::Protocol(_)), "{error}");
}

#[test]
fn format_names_round_trip_through_their_string_form() {
    for format in Format::ALL {
        assert_eq!(format.as_str().parse::<Format>().unwrap(), format);
    }
    assert!("mh".parse::<Format>().is_err());
}
