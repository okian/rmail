//! Attachment search: which arm found it, which attachment it was, and which
//! page.
//!
//! The dense arm is deliberately silent in most of these. `vec_chunks` is only
//! populated by [`crate::index::semantic::SemanticIndex`], which needs a real
//! embedder to say anything meaningful, and the deterministic
//! [`crate::embed::HashEmbedder`] the suite uses instead produces similarities
//! that are stable but arbitrary — so an assertion resting on them would pin a
//! hash, not a ranking. What the arm *is* asserted on is the part that can be
//! made deterministic: [`fuse`], directly, against hand-computed numbers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use super::*;
use crate::attach::{extract_attachments, Provenance};
use crate::config::{IndexExtractConfig, IndexSemanticConfig, SearchConfig};
use crate::embed::hash::HashEmbedder;
use crate::index::extract::Part;
use crate::index::semantic::{SemanticIndex, VECTOR_DIM};
use crate::repo;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A multipart RFC822 message carrying the given attachments, mirroring
/// `attach::tests`' own fixture builder.
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

pub(crate) struct Fixture {
    pub(crate) db: Database,
    pub(crate) account_id: i64,
    pub(crate) mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    pub(crate) async fn open() -> Self {
        Self::named("attach-search").await
    }

    pub(crate) async fn named(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-{tag}-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open");
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
            .expect("seed");
        Self {
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    pub(crate) async fn mailbox(&self, name: &str) -> i64 {
        let account_id = self.account_id;
        let name = name.to_owned();
        self.db
            .write(move |c| {
                repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name,
                        ..Default::default()
                    },
                )
            })
            .await
            .expect("mailbox")
    }

    /// Insert a message carrying `attachments`, extract them, and record the
    /// `attachments` metadata rows a real sync would have written.
    pub(crate) async fn with_attachments(
        &self,
        mailbox_id: i64,
        attachments: &[(&str, &str, &[u8])],
    ) -> i64 {
        let raw = message_with(attachments);
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, mailbox_id);
        let message_id = self
            .db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some("With attachments".to_owned()),
                        from_addr: Some("ada@example.com".to_owned()),
                        raw: Some(raw),
                        date: Some(1_700_000_000 + uid),
                        ..Default::default()
                    },
                )
            })
            .await
            .expect("insert");
        // `attach::decode_parts` names parts positionally, so the metadata
        // rows have to agree — a filename that did not join would silently
        // make every hit anonymous.
        let meta: Vec<(String, String, String, i64)> = attachments
            .iter()
            .enumerate()
            .map(|(index, (filename, content_type, bytes))| {
                (
                    index.to_string(),
                    (*filename).to_owned(),
                    (*content_type).to_owned(),
                    bytes.len() as i64,
                )
            })
            .collect();
        self.db
            .write(move |c| {
                for (part_id, filename, content_type, size) in &meta {
                    c.execute(
                        "INSERT INTO attachments
                             (message_id, part_id, filename, content_type, size)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![message_id, part_id, filename, content_type, size],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("attachment metadata");
        extract_attachments(&self.db, &IndexExtractConfig::default(), message_id)
            .await
            .expect("extract");
        message_id
    }

    pub(crate) async fn set_raw(&self, message_id: i64, raw: Vec<u8>) {
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE messages SET raw = ?2 WHERE id = ?1",
                    rusqlite::params![message_id, raw],
                )
            })
            .await
            .expect("set raw");
    }

    /// Chunk and embed a message the way the indexer does, so `vec_chunks`
    /// is populated and the dense arm has rows to return.
    ///
    /// [`HashEmbedder`] rather than the real ONNX model: its *similarities*
    /// are arbitrary, which is why no test here asserts a dense ranking —
    /// but everything mechanical about the arm (which rows the joins admit,
    /// that a span survives into a hit, that a stale model is excluded) is
    /// deterministic under it, and none of that was exercised at all while
    /// `vec_chunks` was empty.
    pub(crate) async fn embed(&self, message_id: i64) {
        SemanticIndex::new(
            self.db.clone(),
            Arc::new(HashEmbedder::new(VECTOR_DIM)),
            &IndexSemanticConfig::default(),
        )
        .index_message(message_id)
        .await
        .expect("index");
    }

    /// Write an `index_content` row directly, for a part the attachment
    /// pipeline does not produce.
    pub(crate) async fn set_part(&self, message_id: i64, part: &str, text: &str) {
        let (part, text) = (part.to_owned(), text.to_owned());
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO index_content
                         (message_id, part, text, chars, content_hash, extractor)
                     VALUES (?1, ?2, ?3, ?4, x'00', 'test')
                     ON CONFLICT(message_id, part) DO UPDATE SET text = excluded.text",
                    rusqlite::params![message_id, part, text, text.chars().count() as i64],
                )
            })
            .await
            .expect("index_content");
    }

    pub(crate) fn search(&self) -> AttachmentSearch {
        AttachmentSearch::new(
            self.db.clone(),
            Arc::new(HashEmbedder::new(VECTOR_DIM)),
            &SearchConfig::default(),
        )
    }

    pub(crate) fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        self.db
            .with_read(move |c| c.query_row(&sql, [], |r| r.get(0)))
            .expect("count")
    }

    pub(crate) fn text_of(&self, message_id: i64, part_id: &str) -> String {
        let key = format!("attachment:{part_id}");
        self.db
            .with_read(move |c| {
                c.query_row(
                    "SELECT text FROM index_content WHERE message_id = ?1 AND part = ?2",
                    rusqlite::params![message_id, key],
                    |r| r.get(0),
                )
            })
            .expect("text")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn query(text: &str) -> AttachmentQuery {
    AttachmentQuery {
        query: text.to_owned(),
        ..AttachmentQuery::default()
    }
}

async fn run(fx: &Fixture, text: &str) -> Vec<AttachmentHit> {
    fx.search()
        .search(&query(text), &CancellationToken::new())
        .await
        .expect("search")
}

// ---------------------------------------------------------------------------
// Pure
// ---------------------------------------------------------------------------

/// The one duplicated string in this module actually matches the enum it was
/// copied from.
#[test]
fn the_attachment_part_prefix_matches_the_part_key() {
    assert_eq!(
        Part::Attachment("7".to_owned()).as_key(),
        format!("{ATTACHMENT_PART_PREFIX}7")
    );
}

/// RRF against hand-computed values, not merely against an ordering.
///
/// The module brief's own warning applies here as much as it does to
/// `fuse::fuse_scores`: a subtly wrong fusion formula still produces a
/// *plausible* ranking, which is exactly what an ordering-only assertion
/// cannot catch. So the numbers are written out.
#[test]
fn rrf_sums_the_reciprocal_ranks_of_both_arms() {
    let lexical = vec![
        Ranked {
            doc_id: 10,
            score: 3.0,
            span: None,
        },
        Ranked {
            doc_id: 20,
            score: 2.0,
            span: None,
        },
    ];
    let dense = vec![
        Ranked {
            doc_id: 20,
            score: 0.9,
            span: Some((400, 900)),
        },
        Ranked {
            doc_id: 30,
            score: 0.8,
            span: Some((0, 500)),
        },
    ];

    let fused = fuse(&lexical, &dense, 60, 10);
    assert_eq!(fused.len(), 3);

    // 20 is the only document both arms found: 1/(60+2) + 1/(60+1).
    assert_eq!(fused[0].doc_id, 20);
    assert!((fused[0].score - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
    assert_eq!(fused[0].lexical_rank, Some(2));
    assert_eq!(fused[0].dense_rank, Some(1));
    // The dense arm is the one that knows *where*, so its span survives.
    assert_eq!(fused[0].span, Some((400, 900)));

    // 10 was lexical rank 1 and nothing else: 1/61.
    assert_eq!(fused[1].doc_id, 10);
    assert!((fused[1].score - 1.0 / 61.0).abs() < 1e-12);
    assert_eq!(fused[1].dense_rank, None);

    // 30 was dense rank 2 and nothing else: 1/62.
    assert_eq!(fused[2].doc_id, 30);
    assert!((fused[2].score - 1.0 / 62.0).abs() < 1e-12);
    assert_eq!(fused[2].lexical_rank, None);

    // The property that makes fusion worth having at all.
    assert!(
        fused[0].score > fused[1].score,
        "a document both arms found must outrank one only the stronger arm did"
    );
}

/// A page is truncated to `limit` *after* fusion, not before either arm.
#[test]
fn fusion_truncates_to_the_page_after_scoring() {
    let lexical: Vec<Ranked> = (1..=5)
        .map(|doc_id| Ranked {
            doc_id,
            score: 1.0,
            span: None,
        })
        .collect();
    let fused = fuse(&lexical, &[], 60, 2);
    assert_eq!(fused.len(), 2);
    assert_eq!(fused[0].doc_id, 1);
    assert_eq!(fused[1].doc_id, 2);
}

/// The user types a query, not an FTS5 expression.
#[test]
fn match_syntax_in_a_query_is_quoted_rather_than_obeyed() {
    let terms = snippet::query_terms("termination NOT convenience");
    let expression = match_expression(&terms).expect("terms");
    // Every token is a quoted literal, so the bare `NOT` cannot invert the
    // match — it is looked for as a word.
    assert!(expression.contains("\"termination\""), "{expression}");
    assert!(expression.contains("\"NOT\""), "{expression}");
    assert!(!expression.contains(" NOT \""), "{expression}");
}

#[test]
fn a_quote_inside_a_term_is_escaped_rather_than_closing_the_literal() {
    assert_eq!(quote_fts(r#"a"b"#), r#""a""b""#);
}

#[test]
fn a_query_with_nothing_indexable_produces_no_lexical_expression() {
    assert_eq!(match_expression(&snippet::query_terms("--- ...")), None);
}

/// A window cut mid-character yields the valid interior, never a replacement
/// character — a quote's whole value is that it is verbatim.
#[test]
fn a_window_cut_mid_character_keeps_only_what_is_really_there() {
    let text = "aé€b";
    let bytes = text.as_bytes();
    // Start one byte into the two-byte `é`, end one byte into the three-byte
    // `€`.
    let cut = bytes.get(2..bytes.len() - 2).unwrap_or_default();
    let (skipped, window) = decode_window(cut);
    assert!(
        !window.contains('\u{fffd}'),
        "the window invented a character: {window:?}"
    );
    assert!(text.contains(window), "{window:?} is not in {text:?}");
    // The skipped count is what a caller reporting a byte span has to add
    // back; without it the span is shifted at both ends and `span_start` is
    // not even a character boundary.
    assert_eq!(skipped, 1);
    assert_eq!(
        text.get(2 + skipped..2 + skipped + window.len()),
        Some(window),
        "the reported offset does not describe the text that was returned"
    );
}

// ---------------------------------------------------------------------------
// Against the real index
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_query_finds_the_attachment_that_contains_it_not_its_sibling() {
    let fx = Fixture::open().await;
    let message_id = fx
        .with_attachments(
            fx.mailbox_id,
            &[
                (
                    "contract.txt",
                    "text/plain",
                    b"Either party may terminate this agreement for convenience on \
                      thirty days written notice." as &[u8],
                ),
                (
                    "menu.txt",
                    "text/plain",
                    b"Tuesday lunch menu: soup, sandwiches, and a seasonal salad." as &[u8],
                ),
            ],
        )
        .await;

    let hits = run(&fx, "terminate for convenience").await;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].message_id, message_id);
    // The point of the whole feature: *which* attachment, not just which mail.
    assert_eq!(hits[0].part_id, "0");
    assert_eq!(hits[0].filename, "contract.txt");
    assert_eq!(hits[0].mailbox, "INBOX");
    assert_eq!(hits[0].account_id, fx.account_id);
    assert_eq!(hits[0].provenance, Provenance::Native);
    assert_eq!(hits[0].lexical_rank, Some(1));
    assert!(hits[0].score > 0.0);
    fx.stop();
}

#[tokio::test]
async fn a_hit_names_the_page_the_phrase_is_on() {
    let fx = Fixture::open().await;
    let pdf = crate::attach::extract::tests::pdf_bytes(&[
        "Recitals and definitions for the parties involved",
        "Either party may terminate this agreement for convenience",
        "Signatures and counterparts of the parties",
    ]);
    fx.with_attachments(fx.mailbox_id, &[("contract.pdf", "application/pdf", &pdf)])
        .await;

    let hits = run(&fx, "\"terminate this agreement for convenience\"").await;
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].pages, Some(3));
    assert_eq!(
        hits[0].page,
        Some(2),
        "the clause is on page two; hit was {:?}",
        hits[0]
    );
    fx.stop();
}

#[tokio::test]
async fn the_excerpt_is_verbatim_from_the_attachment() {
    let fx = Fixture::open().await;
    let message_id = fx
        .with_attachments(
            fx.mailbox_id,
            &[(
                "invoice.txt",
                "text/plain",
                "Invoice INV-9 for hosting: the total due is 4200 dollars, payable in \
                 thirty days. Café charges are included."
                    .as_bytes(),
            )],
        )
        .await;

    let hits = run(&fx, "hosting total").await;
    assert_eq!(hits.len(), 1, "{hits:?}");
    let stored = fx.text_of(message_id, "0");
    let quoted = hits[0].excerpt.replace('…', "");
    assert!(!quoted.trim().is_empty(), "no excerpt at all");
    assert!(
        stored.contains(quoted.trim()),
        "excerpt {quoted:?} is not a substring of the stored text {stored:?}"
    );
    fx.stop();
}

#[tokio::test]
async fn an_attachment_that_stops_extracting_stops_being_findable() {
    let fx = Fixture::open().await;
    let message_id = fx
        .with_attachments(
            fx.mailbox_id,
            &[(
                "contract.txt",
                "text/plain",
                b"Termination for convenience is permitted." as &[u8],
            )],
        )
        .await;
    assert_eq!(run(&fx, "termination").await.len(), 1);
    assert_eq!(fx.count("attachment_docs"), 1);

    // Replaced by something no extractor reads — the "encrypted version"
    // case. Its text must stop answering queries, not merely stop being
    // refreshed.
    fx.set_raw(
        message_id,
        message_with(&[(
            "contract.bin",
            "application/octet-stream",
            b"\x00\x01\x02\x03",
        )]),
    )
    .await;
    extract_attachments(&fx.db, &IndexExtractConfig::default(), message_id)
        .await
        .expect("re-extract");

    assert!(
        run(&fx, "termination").await.is_empty(),
        "an attachment with no text is still findable by what it used to say"
    );
    assert_eq!(fx.count("attachment_docs"), 0);
    fx.stop();
}

#[tokio::test]
async fn deleting_a_message_takes_its_attachments_out_of_the_index() {
    let fx = Fixture::open().await;
    let message_id = fx
        .with_attachments(
            fx.mailbox_id,
            &[(
                "contract.txt",
                "text/plain",
                b"Termination for convenience is permitted." as &[u8],
            )],
        )
        .await;
    assert_eq!(run(&fx, "termination").await.len(), 1);

    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [message_id]))
        .await
        .expect("delete");

    // A virtual table takes no foreign key, so the trigger is the only thing
    // between a deleted contract and a search that still returns it.
    assert!(run(&fx, "termination").await.is_empty());
    assert_eq!(fx.count("attachment_docs"), 0);
    assert_eq!(fx.count("fts_attachments"), 0);
    fx.stop();
}

#[tokio::test]
async fn a_search_scoped_to_one_message_ignores_the_others() {
    let fx = Fixture::open().await;
    let first = fx
        .with_attachments(
            fx.mailbox_id,
            &[(
                "a.txt",
                "text/plain",
                b"Termination for convenience, first copy." as &[u8],
            )],
        )
        .await;
    fx.with_attachments(
        fx.mailbox_id,
        &[(
            "b.txt",
            "text/plain",
            b"Termination for convenience, second copy." as &[u8],
        )],
    )
    .await;

    assert_eq!(run(&fx, "termination").await.len(), 2);
    let scoped = fx
        .search()
        .search(
            &AttachmentQuery {
                query: "termination".to_owned(),
                message_id: first,
                ..AttachmentQuery::default()
            },
            &CancellationToken::new(),
        )
        .await
        .expect("search");
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].message_id, first);
    fx.stop();
}

#[tokio::test]
async fn a_search_scoped_to_one_account_ignores_the_others() {
    let fx = Fixture::open().await;
    fx.with_attachments(
        fx.mailbox_id,
        &[(
            "a.txt",
            "text/plain",
            b"Termination for convenience." as &[u8],
        )],
    )
    .await;

    let hits = fx
        .search()
        .search(
            &AttachmentQuery {
                query: "termination".to_owned(),
                account_id: fx.account_id + 999,
                ..AttachmentQuery::default()
            },
            &CancellationToken::new(),
        )
        .await
        .expect("search");
    assert!(hits.is_empty(), "another account's mail leaked: {hits:?}");
    fx.stop();
}

// ---------------------------------------------------------------------------
// The dense arm, with rows in `vec_chunks`
// ---------------------------------------------------------------------------

/// A query with no lexical overlap at all still finds the attachment, and the
/// hit carries the chunk's own span — the offset a page is resolved from.
#[tokio::test]
async fn a_dense_only_hit_carries_the_chunk_span_that_found_it() {
    let fx = Fixture::open().await;
    let message_id = fx
        .with_attachments(
            fx.mailbox_id,
            &[(
                "contract.txt",
                "text/plain",
                "Either party may terminate this agreement for convenience upon thirty \
                 days written notice to the other party."
                    .as_bytes(),
            )],
        )
        .await;
    fx.embed(message_id).await;

    // Shares no word with the document, so the lexical arm is empty by
    // construction and anything returned came from the kNN.
    let hits = run(&fx, "zzzqqxjunobtainium").await;
    assert_eq!(hits.len(), 1, "the dense arm returned nothing: {hits:?}");
    assert_eq!(hits[0].message_id, message_id);
    assert_eq!(hits[0].part_id, "0");
    assert_eq!(hits[0].lexical_rank, None);
    assert_eq!(hits[0].dense_rank, Some(1));
    assert!(
        hits[0].span_end > hits[0].span_start,
        "a dense hit's span is a chunk, not a point: {:?}",
        hits[0]
    );
    // No page: only PDF extraction and the OCR path record page spans, so a
    // plain-text attachment has none. This is precisely the case the byte
    // span exists for — a citation into this document can say *where* even
    // though it can never say *which page*.
    assert_eq!(hits[0].page, None);
    fx.stop();
}

/// The kNN is over *attachment* chunks. A body chunk is in the same table and
/// must not surface here — there is no attachment for it to name.
#[tokio::test]
async fn a_body_chunk_is_not_an_attachment_hit() {
    let fx = Fixture::open().await;
    let message_id = fx
        .with_attachments(
            fx.mailbox_id,
            &[(
                "noise.txt",
                "text/plain",
                b"Unrelated filler text about nothing at all." as &[u8],
            )],
        )
        .await;
    fx.set_part(
        message_id,
        "body",
        "Either party may terminate this agreement for convenience upon thirty days \
         written notice.",
    )
    .await;
    fx.embed(message_id).await;

    // The phrase is only in the body, so the lexical arm cannot match it and
    // a dense hit on the body chunk is the only way it could be returned.
    for hit in run(&fx, "\"terminate this agreement for convenience\"").await {
        assert_eq!(
            hit.part_id, "0",
            "a body chunk was returned as an attachment: {hit:?}"
        );
    }
    fx.stop();
}

/// A vector from another model is not comparable to this one's queries, so it
/// is excluded rather than ranked — the same guard `index::semantic` applies.
#[tokio::test]
async fn a_chunk_embedded_by_another_model_is_not_a_dense_hit() {
    let fx = Fixture::open().await;
    let message_id = fx
        .with_attachments(
            fx.mailbox_id,
            &[(
                "contract.txt",
                "text/plain",
                b"Either party may terminate this agreement for convenience." as &[u8],
            )],
        )
        .await;
    fx.embed(message_id).await;
    assert_eq!(run(&fx, "zzzqqxjunobtainium").await.len(), 1);

    fx.db
        .write(|c| c.execute("UPDATE chunk_embeddings SET model = 'some-other-model'", []))
        .await
        .expect("restamp");

    assert!(
        run(&fx, "zzzqqxjunobtainium").await.is_empty(),
        "a vector from another model was ranked against this model's query"
    );
    fx.stop();
}

/// When both arms find a document, the *better-ranked* arm places the
/// evidence. A dense span that always won would page a phrase match from
/// wherever the embedding happened to land.
#[test]
fn the_better_ranked_arm_places_the_evidence() {
    let lexical = vec![Ranked {
        doc_id: 1,
        score: 3.0,
        span: None,
    }];
    let dense = vec![
        Ranked {
            doc_id: 9,
            score: 0.9,
            span: Some((0, 100)),
        },
        Ranked {
            doc_id: 1,
            score: 0.8,
            span: Some((5_000, 5_500)),
        },
    ];
    let fused = fuse(&lexical, &dense, 60, 10);
    let one = fused
        .iter()
        .find(|f| f.doc_id == 1)
        .expect("document 1 was found by both arms");
    assert_eq!(one.lexical_rank, Some(1));
    assert_eq!(one.dense_rank, Some(2));
    assert_eq!(
        one.span, None,
        "the lexical arm ranked it better, so the offset must be located from \
         its literal evidence rather than taken from the chunk"
    );

    // ...and the reverse: dense ranked better, so its span stands.
    let fused = fuse(
        &[Ranked {
            doc_id: 1,
            score: 3.0,
            span: None,
        }],
        &[Ranked {
            doc_id: 1,
            score: 0.9,
            span: Some((5_000, 5_500)),
        }],
        60,
        10,
    );
    assert_eq!(fused[0].span, Some((5_000, 5_500)));
}

#[tokio::test]
async fn an_empty_query_is_an_argument_error() {
    let fx = Fixture::open().await;
    let error = fx
        .search()
        .search(&query("   "), &CancellationToken::new())
        .await
        .expect_err("an empty query is not a query");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    fx.stop();
}

#[tokio::test]
async fn a_cancelled_search_returns_nothing_rather_than_half_a_page() {
    let fx = Fixture::open().await;
    fx.with_attachments(
        fx.mailbox_id,
        &[(
            "a.txt",
            "text/plain",
            b"Termination for convenience." as &[u8],
        )],
    )
    .await;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let hits = fx
        .search()
        .search(&query("termination"), &cancel)
        .await
        .expect("a cancelled search is not an error");
    assert!(hits.is_empty());
    fx.stop();
}

impl Fixture {
    /// Explicit teardown, so a test that panics still names the assertion
    /// rather than a `Drop` that ran during unwinding.
    fn stop(self) {
        drop(self);
    }
}
