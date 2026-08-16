//! Attachment semantic search: which *attachment* answers a query, and which
//! page of it (prd.md feature 55, "so 'the contract clause about termination
//! for convenience' returns the exact attachment and page").
//!
//! ```text
//! query ──┬─▶ BM25 over fts_attachments ──┐
//!         └─▶ kNN over vec_chunks (attachment parts only) ──┤
//!                                                RRF ──▶ locate ──▶ page
//! ```
//!
//! # Why this is not `SearchService.Search` with a filter
//!
//! The message pipeline ranks *messages*, and it is right to: a term in a PDF
//! is evidence about the mail that carried it. This ranks attachments, which
//! is a different question with a different key. A message carrying a signed
//! contract and a signature-block logo is one row in `fts_messages` covering
//! both; nothing about that row can say the clause came from the first, and
//! "the exact attachment and page" is the entire acceptance criterion here.
//!
//! So both arms are resolved to `attachment_docs.doc_id` — one row per
//! extracted attachment — and fused on that key:
//!
//! - **Lexical.** `fts_attachments` (migration V39) is BM25 at exactly that
//!   granularity, written by [`super::persist`] in the same transaction as
//!   the text it indexes.
//! - **Dense.** `vec_chunks` is *already* attachment-granular and finer:
//!   [`crate::index::semantic`] chunks every `index_content` part, including
//!   `attachment:<n>`, and a chunk carries the byte span it came from. This
//!   module adds no chunking and no embedding of its own — it restricts the
//!   existing kNN to attachment parts and dedupes to the best chunk per
//!   attachment, which is where the byte offset a page is resolved from
//!   comes from.
//!
//! # RRF, and why not [`crate::fuse::fuse_scores`]
//!
//! Reciprocal rank fusion is the same formula that module uses and the same
//! `search.rrf_k` tunes. What is not the same is everything around it:
//! `fuse_scores` is keyed by `messages.id`, weights each source by the
//! query's *intent* (a classification this surface never runs), and feeds
//! thread collapse and SimHash dedup that have no meaning for a document.
//! Passing a `doc_id` through a field called `message_id` to inherit
//! machinery none of which applies would be a worse kind of reuse than
//! forty lines of arithmetic with its own hand-computed test.
//!
//! # Locating the page without reading the document
//!
//! An extracted attachment is up to [`crate::attach::extract::MAX_TEXT_BYTES`]
//! — two megabytes — and a page of hits must not pull that per hit. Two
//! things make it unnecessary:
//!
//! - A dense hit already *has* a byte offset (its chunk's `span_start`).
//! - A lexical hit's offset is found by SQLite, not by this process:
//!   `instr(CAST(lower(text) AS BLOB), CAST(lower(?) AS BLOB))` searches the
//!   whole column and returns a **byte** position, because `instr` over two
//!   blobs counts bytes rather than characters, and SQLite's built-in
//!   `lower()` only folds ASCII `A-Z` — a 1:1 byte mapping, so an offset
//!   found in the folded text is valid in the original.
//!
//! Only a bounded window around that offset is ever read, and
//! `attachment_pages` (migration V13) turns the offset into a page number.

use std::collections::BTreeMap;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use tokio_util::sync::CancellationToken;

use crate::config::SearchConfig;
use crate::embed::Embedder;
use crate::error::Error;
use crate::present::snippet;
use crate::retrieve::cancel::interruptible_read;
use crate::storage::Database;

#[cfg(test)]
pub(crate) mod tests;

/// The `index_content.part` prefix attachment text is stored under.
///
/// Duplicated from [`crate::index::extract::Part::as_key`] rather than
/// derived from it, because most uses of it here are inside SQL string
/// literals where interpolating a Rust constant would obscure the query more
/// than it would protect it. `the_attachment_part_prefix_matches_the_part_key`
/// pins the two together.
const ATTACHMENT_PART_PREFIX: &str = "attachment:";

/// Largest page this surface will return, however large a caller asks for.
/// Every hit costs a bounded text window and a page lookup, so the ceiling is
/// on work rather than on bytes.
pub const MAX_LIMIT: u32 = 50;

/// The page size used when a caller asks for none.
pub const DEFAULT_LIMIT: u32 = 20;

/// How far past the requested page each arm reaches.
///
/// Both arms are filtered *after* their own top-N is taken (by account, by
/// message, and — for the dense arm — by model and part kind), so an
/// unwidened fetch spends its budget on rows that never survive. The same
/// reasoning, and the same factor, as [`crate::retrieve::dense`]'s own
/// overfetch.
const OVERFETCH: usize = 8;

/// Ceiling on either arm's fetch, however much widening would ask for.
const MAX_FETCH: usize = 2_000;

/// How much text one hit's excerpt window reads, in bytes.
///
/// Centred on the located offset, so a hit deep inside a fifty-page contract
/// costs the same as one on page one.
const WINDOW_BYTES: usize = 4_096;

/// How far before the located offset the window starts, so an excerpt has
/// some lead-in rather than beginning mid-sentence at the match.
const WINDOW_LEAD: usize = 512;

/// One attachment that matched, with the evidence that placed it.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentHit {
    /// The message carrying the attachment.
    pub message_id: i64,
    /// `messages.uid`, for a client that addresses mail by UID.
    pub message_uid: i64,
    /// Owning account.
    pub account_id: i64,
    /// Owning mailbox name.
    pub mailbox: String,
    /// The carrying message's subject, empty when it has none.
    pub subject: String,
    /// The carrying message's From address, empty when it has none.
    pub from_addr: String,
    /// The carrying message's Date, unix seconds, when it has one.
    pub date: Option<i64>,
    /// The MIME part id, as [`super::AttachmentText::part_id`] records it.
    pub part_id: String,
    /// The attachment's filename, empty when the part declared none.
    pub filename: String,
    /// Its declared content type, empty when the part declared none.
    pub content_type: String,
    /// Its decoded size in bytes, when recorded.
    pub bytes: Option<i64>,
    /// How many pages extraction found, when the format has them.
    pub pages: Option<i64>,
    /// The page the evidence falls on, one-based. `None` for a format with
    /// no pages, or an offset no page span covers.
    pub page: Option<i64>,
    /// Byte offset of the evidence within the attachment's extracted text.
    pub span_start: i64,
    /// Byte offset just past it. Equal to `span_start` when the offset came
    /// from a term match rather than a chunk.
    pub span_end: i64,
    /// A verbatim excerpt of the attachment's own text around the evidence.
    /// Drawn from `index_content`, never from a model.
    pub excerpt: String,
    /// Whether the text was read out of the format or recognized by OCR — a
    /// hit on OCR'd text is real, but it is a guess about pixels.
    pub provenance: super::Provenance,
    /// Fused reciprocal-rank score, higher is better.
    pub score: f64,
    /// This attachment's 1-based rank in the lexical arm, when it appeared.
    pub lexical_rank: Option<u32>,
    /// Its 1-based rank in the dense arm, when it appeared.
    pub dense_rank: Option<u32>,
}

/// What to search, and how much of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachmentQuery {
    /// The natural-language or keyword query.
    pub query: String,
    /// Restrict to one account; `0` means every account.
    pub account_id: i64,
    /// Restrict to one message's attachments; `0` means every message.
    pub message_id: i64,
    /// How many attachments to return; `0` means [`DEFAULT_LIMIT`].
    pub limit: u32,
}

/// Ranked search over extracted attachment text.
///
/// Cheap to clone: a `Database` handle, an `Arc<dyn Embedder>` and two small
/// values.
#[derive(Clone)]
pub struct AttachmentSearch {
    db: Database,
    /// The *same* embedder the daemon indexes with — a query embedded by a
    /// different model than the corpus produces cosines with no meaning,
    /// which is worse than an error because they still sort.
    embedder: Arc<dyn Embedder>,
    /// `search.retrievers.dense`. When off, this surface is lexical-only
    /// rather than erroring, exactly as
    /// [`crate::retrieve::fanout::Fanout`] degrades a disabled source.
    dense: bool,
    /// `search.rrf_k`.
    rrf_k: u32,
    /// `search.candidates_per_source`, the depth each arm reaches to.
    candidates_per_source: u32,
}

impl std::fmt::Debug for AttachmentSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachmentSearch")
            .field("model", &self.embedder.model())
            .field("dense", &self.dense)
            .field("rrf_k", &self.rrf_k)
            .finish_non_exhaustive()
    }
}

impl AttachmentSearch {
    /// Build the surface over the daemon's own database and embedder.
    #[must_use]
    pub fn new(db: Database, embedder: Arc<dyn Embedder>, search: &SearchConfig) -> Self {
        Self {
            db,
            embedder,
            dense: search.retrievers.dense,
            rrf_k: search.rrf_k.max(1),
            candidates_per_source: search.candidates_per_source.max(1),
        }
    }

    /// Rank attachments for `query`, best first.
    ///
    /// An empty result is a success: nothing matched, or nothing has been
    /// extracted yet.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for an empty query. Otherwise whatever the
    /// embedder or a database read failed with. A cancelled read yields an
    /// empty page rather than an error — a superseded search has no answer to
    /// give, and a truncated one presented as complete would be worse.
    #[tracing::instrument(
        skip(self, req, cancel),
        fields(
            account_id = req.account_id,
            message_id = req.message_id,
            lexical = tracing::field::Empty,
            dense = tracing::field::Empty,
            hits = tracing::field::Empty,
        )
    )]
    pub async fn search(
        &self,
        req: &AttachmentQuery,
        cancel: &CancellationToken,
    ) -> Result<Vec<AttachmentHit>, Error> {
        let query = req.query.trim();
        if query.is_empty() {
            return Err(Error::invalid_argument("a query is required"));
        }
        let limit = match req.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        } as usize;
        let fetch = limit
            .saturating_mul(OVERFETCH)
            .max(self.candidates_per_source as usize)
            .min(MAX_FETCH);

        let terms = snippet::query_terms(query);
        let lexical = self.lexical(&terms, req, fetch, cancel).await?;
        let dense = self.dense(query, req, fetch, cancel).await?;

        let span = tracing::Span::current();
        span.record("lexical", lexical.len());
        span.record("dense", dense.len());

        let fused = fuse(&lexical, &dense, self.rrf_k, limit);
        if fused.is_empty() {
            return Ok(Vec::new());
        }
        let hits = self.hydrate(fused, &terms, cancel).await?;
        span.record("hits", hits.len());
        Ok(hits)
    }

    /// The BM25 arm: `fts_attachments`, filtered to the requested scope.
    async fn lexical(
        &self,
        terms: &snippet::QueryTerms,
        req: &AttachmentQuery,
        fetch: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<Ranked>, Error> {
        let Some(expression) = match_expression(terms) else {
            // Nothing indexable to match on — a query of pure operators, or
            // of punctuation. The dense arm may still have something to say.
            return Ok(Vec::new());
        };
        let (account_id, message_id) = (req.account_id, req.message_id);
        let page = i64::try_from(fetch).unwrap_or(i64::MAX);
        let rows = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(
                "SELECT d.doc_id, bm25(fts_attachments)
                 FROM fts_attachments
                 JOIN attachment_docs d ON d.doc_id = fts_attachments.rowid
                 JOIN messages m ON m.id = d.message_id
                 WHERE fts_attachments MATCH ?1
                   AND (?2 = 0 OR m.account_id = ?2)
                   AND (?3 = 0 OR m.id = ?3)
                 ORDER BY bm25(fts_attachments)
                 LIMIT ?4",
            )?;
            let rows = stmt
                .query_map(
                    rusqlite::params![expression, account_id, message_id, page],
                    |row| {
                        Ok(Ranked {
                            doc_id: row.get(0)?,
                            // BM25 is negative-is-better in SQLite, so the
                            // `ORDER BY` needs no `DESC`; every score that
                            // leaves this module is higher-is-better, the same
                            // orientation `index::fts` hands out.
                            score: -row.get::<_, f64>(1)?,
                            span: None,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(malformed_query)?;
        Ok(rows.unwrap_or_default())
    }

    /// The kNN arm: `vec_chunks`, restricted to attachment parts and deduped
    /// to the best-matching chunk of each attachment.
    async fn dense(
        &self,
        query: &str,
        req: &AttachmentQuery,
        fetch: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<Ranked>, Error> {
        if !self.dense {
            return Ok(Vec::new());
        }
        let embedded = self.embedder.embed(&[query.to_owned()]).await?;
        let Some(vector) = embedded.into_iter().next() else {
            return Ok(Vec::new());
        };
        if vector.as_slice().iter().all(|value| *value == 0.0) {
            // Nothing is near a point with no direction; a kNN from the origin
            // is an arbitrary set presented as an answer. The same guard
            // `index::semantic::search_vector` applies.
            return Ok(Vec::new());
        }
        let bytes = vector.to_bytes();
        let model = self.embedder.model().to_owned();
        let dim = i64::try_from(self.embedder.dim()).unwrap_or(i64::MAX);
        let (account_id, message_id) = (req.account_id, req.message_id);
        let k = i64::try_from(fetch).unwrap_or(i64::MAX);
        let attachment_prefix = format!("{ATTACHMENT_PART_PREFIX}%");

        let rows = interruptible_read(&self.db, cancel, move |conn| {
            // The kNN happens inside `vec_chunks`; everything outside the CTE
            // narrows it. `k` slots are spent before those filters run, which
            // is exactly what the overfetch above compensates for — the same
            // trade `retrieve::dense` documents.
            let mut stmt = conn.prepare(
                "WITH hits AS (
                     SELECT chunk_id, distance FROM vec_chunks
                     WHERE embedding MATCH ?1 AND k = ?2
                 )
                 SELECT d.doc_id, c.span_start, c.span_end, h.distance
                 FROM hits h
                 JOIN chunks c ON c.chunk_id = h.chunk_id
                 JOIN chunk_embeddings e ON e.chunk_id = h.chunk_id
                 JOIN attachment_docs d
                   ON d.message_id = c.message_id
                  AND ('attachment:' || d.part_id) = c.part
                 JOIN messages m ON m.id = c.message_id
                 WHERE c.part LIKE ?3
                   AND e.model = ?4 AND e.dim = ?5
                   AND e.content_hash = c.content_hash
                   AND (?6 = 0 OR m.account_id = ?6)
                   AND (?7 = 0 OR m.id = ?7)
                 ORDER BY h.distance",
            )?;
            let rows = stmt
                .query_map(
                    rusqlite::params![
                        bytes,
                        k,
                        attachment_prefix,
                        model,
                        dim,
                        account_id,
                        message_id
                    ],
                    |row| {
                        let distance: f64 = row.get(3)?;
                        Ok(Ranked {
                            doc_id: row.get(0)?,
                            // `vec0`'s metric over unit vectors is L2, and
                            // L2² = 2 − 2·cos. Converted here so every score
                            // this module produces is a cosine, higher better.
                            score: 1.0 - distance * distance / 2.0,
                            span: Some((row.get(1)?, row.get(2)?)),
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;

        // Best chunk per attachment. Max rather than mean, for the reason
        // `retrieve::dense` gives: one strongly-matching clause in a long
        // contract should surface the contract, not be averaged away by the
        // forty pages of boilerplate around it.
        let mut best: BTreeMap<i64, Ranked> = BTreeMap::new();
        for row in rows.unwrap_or_default() {
            match best.get(&row.doc_id) {
                Some(seen) if seen.score >= row.score => {}
                _ => {
                    best.insert(row.doc_id, row);
                }
            }
        }
        let mut deduped: Vec<Ranked> = best.into_values().collect();
        deduped.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.doc_id.cmp(&b.doc_id))
        });
        Ok(deduped)
    }

    /// Turn fused doc ids into presentable hits: metadata, a located offset,
    /// its page, and a verbatim excerpt.
    ///
    /// One connection, one pass. A page of hits is a handful of prepared
    /// statements against local SQLite, and doing it in a single
    /// [`interruptible_read`] keeps the whole hydration cancellable rather
    /// than only its first query.
    async fn hydrate(
        &self,
        fused: Vec<Fused>,
        terms: &snippet::QueryTerms,
        cancel: &CancellationToken,
    ) -> Result<Vec<AttachmentHit>, Error> {
        let needles = needles(terms);
        let terms = terms.clone();
        let rows = interruptible_read(&self.db, cancel, move |conn| {
            let mut out: Vec<AttachmentHit> = Vec::with_capacity(fused.len());
            for item in &fused {
                let Some(row) = read_doc(conn, item.doc_id)? else {
                    // The attachment went away between ranking and now — a
                    // re-fetch replaced the message's raw, say. Not an error;
                    // the page is simply one shorter.
                    continue;
                };
                let span = match item.span {
                    Some(span) => Some(span),
                    None => locate(conn, item.doc_id, &needles)?.map(|at| (at, at)),
                };
                let (span_start, span_end) = span.unwrap_or((0, 0));
                let page = page_at(conn, row.message_id, &row.part_id, span_start)?;
                let excerpt = excerpt(conn, item.doc_id, span_start, &terms)?;
                out.push(AttachmentHit {
                    message_id: row.message_id,
                    message_uid: row.message_uid,
                    account_id: row.account_id,
                    mailbox: row.mailbox,
                    subject: row.subject,
                    from_addr: row.from_addr,
                    date: row.date,
                    part_id: row.part_id,
                    filename: row.filename,
                    content_type: row.content_type,
                    bytes: row.bytes,
                    pages: row.pages,
                    page,
                    span_start,
                    span_end,
                    excerpt,
                    provenance: row.provenance,
                    score: item.score,
                    lexical_rank: item.lexical_rank,
                    dense_rank: item.dense_rank,
                });
            }
            Ok(out)
        })
        .await?;
        Ok(rows.unwrap_or_default())
    }
}

/// One arm's hit: which attachment, how well, and (dense only) where.
#[derive(Debug, Clone, PartialEq)]
struct Ranked {
    doc_id: i64,
    score: f64,
    /// Byte span of the matching chunk, for the arm that has one.
    span: Option<(i64, i64)>,
}

/// One attachment after fusion.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Fused {
    doc_id: i64,
    score: f64,
    span: Option<(i64, i64)>,
    lexical_rank: Option<u32>,
    dense_rank: Option<u32>,
}

/// Reciprocal rank fusion over the two arms, best first.
///
/// `1 / (k + rank)` per arm, summed — the formula
/// [`crate::fuse::fuse_scores`] uses and `search.rrf_k` tunes, over
/// `attachment_docs.doc_id` instead of `messages.id`. Unweighted on purpose:
/// the intent classifier that produces per-source weights for the message
/// pipeline is not run here, and inventing a weight with nothing to justify
/// it would bias the arm that happens to be listed first.
fn fuse(lexical: &[Ranked], dense: &[Ranked], rrf_k: u32, limit: usize) -> Vec<Fused> {
    let k = f64::from(rrf_k.max(1));
    let mut fused: BTreeMap<i64, Fused> = BTreeMap::new();

    for (index, hit) in lexical.iter().enumerate() {
        let rank = index + 1;
        let entry = fused.entry(hit.doc_id).or_insert(Fused {
            doc_id: hit.doc_id,
            score: 0.0,
            span: None,
            lexical_rank: None,
            dense_rank: None,
        });
        entry.score += 1.0 / (k + rank as f64);
        entry.lexical_rank = Some(u32::try_from(rank).unwrap_or(u32::MAX));
    }
    for (index, hit) in dense.iter().enumerate() {
        let rank = index + 1;
        let entry = fused.entry(hit.doc_id).or_insert(Fused {
            doc_id: hit.doc_id,
            score: 0.0,
            span: None,
            lexical_rank: None,
            dense_rank: None,
        });
        entry.score += 1.0 / (k + rank as f64);
        let dense_rank = u32::try_from(rank).unwrap_or(u32::MAX);
        entry.dense_rank = Some(dense_rank);
        // The dense arm is the only one that carries a span, but it must not
        // *always* win it. A contract that matches an exact phrase on page 40
        // and embeds best on page 2 would otherwise be paged from the chunk:
        // the citation names page 2, and the excerpt — centred on an offset
        // where none of the query's words occur — falls back to the
        // document's opening. That is feature 55's own acceptance criterion
        // failing on the query it was written for. So the span is kept only
        // when the dense arm actually ranked this document at least as well;
        // otherwise `hydrate` locates the literal evidence itself.
        //
        // `None < Some(_)` under `Option`'s ordering, so the `is_none` arm is
        // spelled out rather than folded into the comparison.
        if entry.lexical_rank.is_none() || entry.lexical_rank >= Some(dense_rank) {
            entry.span = hit.span;
        }
    }

    let mut out: Vec<Fused> = fused.into_values().collect();
    // Ties broken by doc id so a page is stable across identical calls;
    // `total_cmp` rather than `partial_cmp` because a NaN score would
    // otherwise make the sort's ordering — and therefore which results a
    // caller never sees — undefined.
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.doc_id.cmp(&b.doc_id))
    });
    out.truncate(limit);
    out
}

/// The FTS5 match expression for a parsed query, or `None` when there is
/// nothing indexable to match.
///
/// Every term and phrase is emitted as a quoted FTS5 string, with embedded
/// quotes doubled. That is what keeps a user's `AND`, `NOT`, `*` or `(` from
/// being read as match syntax — this surface takes a *query*, not an FTS5
/// expression, and a stray `NOT` in a sentence must not silently invert it.
/// Terms are `AND`ed, the same conjunction `retrieve::lexical` builds.
fn match_expression(terms: &snippet::QueryTerms) -> Option<String> {
    let mut parts: Vec<String> = Vec::with_capacity(terms.terms.len() + terms.phrases.len());
    for term in &terms.terms {
        parts.push(quote_fts(term));
    }
    for phrase in &terms.phrases {
        parts.push(quote_fts(phrase));
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" AND "))
}

/// One FTS5 string literal.
fn quote_fts(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// The literal strings a lexical hit's offset is looked for, longest first.
///
/// Longest first because a phrase is a more precise statement about where the
/// answer is than any of the words in it, and the first match found wins.
fn needles(terms: &snippet::QueryTerms) -> Vec<String> {
    let mut out: Vec<String> = terms
        .phrases
        .iter()
        .chain(terms.terms.iter())
        .filter(|text| !text.trim().is_empty())
        .cloned()
        .collect();
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out.dedup();
    out
}

/// One attachment's metadata.
struct DocRow {
    message_id: i64,
    message_uid: i64,
    account_id: i64,
    mailbox: String,
    subject: String,
    from_addr: String,
    date: Option<i64>,
    part_id: String,
    filename: String,
    content_type: String,
    bytes: Option<i64>,
    pages: Option<i64>,
    provenance: super::Provenance,
}

/// Everything about one attachment except its text.
fn read_doc(conn: &Connection, doc_id: i64) -> rusqlite::Result<Option<DocRow>> {
    conn.prepare_cached(
        "SELECT d.message_id, d.part_id, m.uid, m.account_id, m.subject, m.from_addr, m.date,
                mb.name, a.filename, a.content_type, a.size, ax.pages, ax.provenance
         FROM attachment_docs d
         JOIN messages m ON m.id = d.message_id
         LEFT JOIN mailboxes mb ON mb.id = m.mailbox_id
         LEFT JOIN attachments a ON a.message_id = d.message_id AND a.part_id = d.part_id
         LEFT JOIN attachment_extractions ax
                ON ax.message_id = d.message_id AND ax.part_id = d.part_id
         WHERE d.doc_id = ?1",
    )?
    .query_row([doc_id], |row| {
        let provenance: Option<String> = row.get(12)?;
        Ok(DocRow {
            message_id: row.get(0)?,
            part_id: row.get(1)?,
            message_uid: row.get(2)?,
            account_id: row.get(3)?,
            subject: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            from_addr: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            date: row.get(6)?,
            mailbox: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
            filename: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
            content_type: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
            bytes: row.get(10)?,
            pages: row.get(11)?,
            // A missing or unrecognized value reads as `Ocr`, the direction
            // that does not over-promise — see `attach::stored`'s own note.
            provenance: provenance
                .and_then(|value| super::Provenance::parse(&value).ok())
                .unwrap_or(super::Provenance::Ocr),
        })
    })
    .optional()
}

/// The byte offset of the first `needle` in this attachment's text, or `None`
/// when none of them occurs in it.
///
/// `instr` over two **blobs** counts bytes, not characters, and SQLite's
/// built-in `lower()` folds only ASCII `A-Z` — a 1:1 byte mapping — so the
/// offset it reports in the folded text is a valid offset in the original.
/// Done in SQLite rather than here because the alternative is pulling up to
/// two megabytes of contract into this process per hit.
fn locate(conn: &Connection, doc_id: i64, needles: &[String]) -> rusqlite::Result<Option<i64>> {
    for needle in needles {
        let lowered = needle.to_lowercase();
        let at: Option<i64> = conn
            .prepare_cached(
                "SELECT instr(CAST(lower(ic.text) AS BLOB), CAST(?2 AS BLOB))
                 FROM attachment_docs d
                 JOIN index_content ic
                   ON ic.message_id = d.message_id
                  AND ic.part = 'attachment:' || d.part_id
                 WHERE d.doc_id = ?1",
            )?
            .query_row(rusqlite::params![doc_id, lowered], |row| row.get(0))
            .optional()?;
        // `instr` is 1-based and returns 0 for "not found".
        if let Some(found) = at.filter(|found| *found > 0) {
            return Ok(Some(found - 1));
        }
    }
    Ok(None)
}

/// Which page a byte offset falls on, or `None` for a format with no pages.
///
/// The synchronous twin of [`super::page_at`], for callers already inside a
/// blocking closure.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn page_at(
    conn: &Connection,
    message_id: i64,
    part_id: &str,
    offset: i64,
) -> rusqlite::Result<Option<i64>> {
    conn.prepare_cached(
        "SELECT page FROM attachment_pages
         WHERE message_id = ?1 AND part_id = ?2
           AND span_start <= ?3 AND span_end > ?3
         ORDER BY page LIMIT 1",
    )?
    .query_row(rusqlite::params![message_id, part_id, offset], |row| {
        row.get(0)
    })
    .optional()
}

/// A verbatim excerpt of the attachment's own text around `offset`.
///
/// Read as a bounded blob rather than as text: `substr` over a BLOB counts
/// bytes, which is the unit every offset here is in, while `substr` over TEXT
/// counts characters and would drift from the spans the moment a document
/// contained one non-ASCII character.
fn excerpt(
    conn: &Connection,
    doc_id: i64,
    offset: i64,
    terms: &snippet::QueryTerms,
) -> rusqlite::Result<String> {
    let start = offset.saturating_sub(WINDOW_LEAD as i64).max(0);
    let bytes: Option<Vec<u8>> = conn
        .prepare_cached(
            "SELECT substr(CAST(ic.text AS BLOB), ?2, ?3)
             FROM attachment_docs d
             JOIN index_content ic
               ON ic.message_id = d.message_id
              AND ic.part = 'attachment:' || d.part_id
             WHERE d.doc_id = ?1",
        )?
        .query_row(
            // `substr` is 1-based over blobs as it is over text.
            rusqlite::params![doc_id, start + 1, WINDOW_BYTES as i64],
            |row| row.get(0),
        )
        .optional()?;
    let Some(bytes) = bytes else {
        return Ok(String::new());
    };
    // The offset is discarded here on purpose: an excerpt is display text,
    // and this module reports the *evidence* offset as `span_start` rather
    // than the window's. `attach::ask` is the caller that has to add it back.
    let (_, window) = decode_window(&bytes);
    // The same snippet machinery every search hit's excerpt comes from, so a
    // quote here and a quote there are the same kind of object. The fallback
    // is a plain leading excerpt, for the semantic hit that shares no literal
    // word with the query at all.
    let snippet = snippet::extract(window, &terms.terms, &terms.phrases)
        .unwrap_or_else(|| snippet::plain_excerpt(window));
    Ok(snippet.text)
}

/// The valid UTF-8 core of a byte window cut at arbitrary offsets.
///
/// Both ends can land inside a multi-byte character. Trimming to the valid
/// interior — rather than decoding lossily — is what keeps the excerpt a
/// genuine substring of the attachment: a `U+FFFD` stitched over a cut
/// character would be a character the document does not contain, in a quote
/// whose entire value is that it is verbatim.
///
/// Returns how many leading bytes were skipped alongside the text, because a
/// caller that reports a byte span for the window has to add it back — see
/// [`super::ask`]'s `fetch_windows`, which does exactly that.
///
/// `pub(super)` so [`super::ask`] packs prompt excerpts through the same
/// decoder: a passage that reached a model with a replacement character in it
/// would be a passage no citation could be checked against.
pub(super) fn decode_window(bytes: &[u8]) -> (usize, &str) {
    // A UTF-8 continuation byte is `10xxxxxx`; skipping them finds the first
    // character start.
    let mut from = 0usize;
    while from < bytes.len() && from < 4 && bytes[from] & 0b1100_0000 == 0b1000_0000 {
        from += 1;
    }
    let tail = bytes.get(from..).unwrap_or_default();
    let text = match std::str::from_utf8(tail) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(tail.get(..error.valid_up_to()).unwrap_or_default())
            .unwrap_or_default(),
    };
    (from, text)
}

/// Record one attachment's text in the attachment-granular lexical index.
///
/// Called from [`super::persist`] inside the same transaction as the
/// `index_content` write, so the text and the index that finds it can never
/// disagree — see that call site's own comment for why anywhere else is
/// unrepairable.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn index_part(
    conn: &Connection,
    message_id: i64,
    part_id: &str,
    text: &str,
) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO attachment_docs (message_id, part_id) VALUES (?1, ?2)
         ON CONFLICT(message_id, part_id) DO NOTHING",
    )?
    .execute(rusqlite::params![message_id, part_id])?;
    let doc_id: i64 = conn.query_row(
        "SELECT doc_id FROM attachment_docs WHERE message_id = ?1 AND part_id = ?2",
        rusqlite::params![message_id, part_id],
        |row| row.get(0),
    )?;
    // Delete-then-insert: a contentless FTS5 table has no upsert, and an
    // insert onto an occupied rowid is a constraint failure rather than a
    // replacement. `contentless_delete = 1` is what makes the delete possible
    // without handing the old text back.
    conn.execute("DELETE FROM fts_attachments WHERE rowid = ?1", [doc_id])?;
    conn.execute(
        "INSERT INTO fts_attachments (rowid, text) VALUES (?1, ?2)",
        rusqlite::params![doc_id, text],
    )?;
    Ok(())
}

/// Remove an attachment from the lexical index.
///
/// Called for a part that produced no text this pass and for one the message
/// no longer has. Both matter: an attachment replaced by an encrypted version
/// must stop being findable by what it used to say.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn forget_part(
    conn: &Connection,
    message_id: i64,
    part_id: &str,
) -> rusqlite::Result<()> {
    let doc_id: Option<i64> = conn
        .query_row(
            "SELECT doc_id FROM attachment_docs WHERE message_id = ?1 AND part_id = ?2",
            rusqlite::params![message_id, part_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(doc_id) = doc_id else {
        return Ok(());
    };
    conn.execute("DELETE FROM fts_attachments WHERE rowid = ?1", [doc_id])?;
    conn.execute("DELETE FROM attachment_docs WHERE doc_id = ?1", [doc_id])?;
    Ok(())
}

/// Map a malformed FTS5 expression to an argument error rather than a server
/// fault — the same treatment [`crate::index::fts`] gives its own.
///
/// [`match_expression`] quotes everything it emits, so this should be
/// unreachable; it stays because "should be unreachable" is not a reason to
/// report a user's query as an internal error if it ever is.
fn malformed_query(error: crate::storage::StorageError) -> Error {
    match error {
        crate::storage::StorageError::Sqlite(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
            if msg.contains("fts5") =>
        {
            Error::invalid_argument(format!("the attachment search query is not valid: {msg}"))
        }
        other => Error::from(other),
    }
}
