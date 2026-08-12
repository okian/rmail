//! What Stage 5 actually reads: the bounded `(subject, sender, date, body
//! excerpt)` text a reranker scores against, fetched once per query for the
//! whole top-K window.
//!
//! # Why the L2 stage fetches its own text
//!
//! Stages 1-4 never load message text. Retrieval works off the FTS index and
//! the vector table, fusion off `messages` metadata, and
//! [`crate::features::FeatureExtractor`] off precomputed scores — by design,
//! since a hundreds-of-candidates pipeline that read bodies would spend its
//! entire latency budget in SQLite. Stage 5 is the first stage that has to
//! read the actual document, and prd.md is explicit that this is the point:
//! "a heavier model that reads actual text, not just features." It is
//! affordable here and only here because the window is already cut to
//! `search.top_k_rerank` (default 50).
//!
//! [`crate::present::Presenter`] also reads bodies, for snippets — but it
//! runs *after* Stage 5 and over a `limit`-sized page, so there is no fetch
//! to share: by the time the presenter runs, the rerank that would have
//! reused its rows has already had to happen. Duplicating a bounded
//! `SUBSTR` read is the cheaper of the two costs.
//!
//! # Degradation is silent and total
//!
//! Every failure path here returns an empty map, never an error: a candidate
//! with no document is one the reranker cannot judge, and
//! [`super::L2Stage`]'s contract is that a rerank which cannot run leaves the
//! L1 order alone. A partial fetch (some rows missing, a cancelled scan) is
//! reported to the caller as "not every candidate has a document," which is
//! what makes the stage skip rather than reorder against a document set that
//! silently lost half its rows.

use std::collections::BTreeMap;

use tokio_util::sync::CancellationToken;

use crate::retrieve::cancel::interruptible_read;
use crate::storage::Database;

/// How much body text one candidate contributes to its rerank document.
///
/// A cross-encoder truncates at its own sequence length (512 tokens for
/// `bge-reranker-base`) and Claude is billed per token, so the cut is made
/// here — once, identically for both backends — rather than left to whichever
/// backend happens to run. 1,200 characters is roughly 300 tokens, which
/// leaves room for the subject/sender header line inside a 512-token window.
pub(super) const MAX_BODY_CHARS: usize = 1_200;

/// Hard ceiling on how many candidates one fetch will look up, mirroring
/// [`crate::present`]'s own cap. `search.top_k_rerank` is the real bound
/// (default 50) and is applied by the caller; this exists so a
/// misconfiguration cannot turn one query into an unbounded `IN (...)` list.
pub(super) const MAX_FETCH: usize = 200;

/// One candidate's rerank document: everything a reranker is allowed to see
/// about a message, already bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Document {
    /// `messages.subject`, empty when the message has none.
    pub(super) subject: String,
    /// The sender as `Name <addr>`, `addr`, or empty — whichever the row has.
    pub(super) sender: String,
    /// `messages.date` as unix seconds, when the row has one.
    pub(super) date: Option<i64>,
    /// The first [`MAX_BODY_CHARS`] characters of the indexed body text.
    pub(super) body: String,
    /// The owning account's configured name, for
    /// [`crate::ai::PolicyEngine`]. Never rendered into a prompt — this is
    /// the *gate*, not content. Empty when the join found no row, which
    /// [`super::L2Stage`] treats as "no policy could be resolved" and
    /// refuses to send.
    pub(super) account: String,
    /// The owning mailbox's name, for the same reason.
    pub(super) mailbox: String,
}

impl Document {
    /// The single text blob a reranker scores. One shape for both backends:
    /// a cross-encoder's `(query, document)` pair and a Claude listwise
    /// prompt entry must describe the same message the same way, or the two
    /// backends silently rank different corpora.
    pub(super) fn render(&self) -> String {
        let mut out = String::with_capacity(self.body.len() + 128);
        if !self.subject.is_empty() {
            out.push_str("Subject: ");
            out.push_str(&self.subject);
            out.push('\n');
        }
        if !self.sender.is_empty() {
            out.push_str("From: ");
            out.push_str(&self.sender);
            out.push('\n');
        }
        if let Some(date) = self.date {
            if let Some(dt) = chrono::DateTime::from_timestamp(date, 0) {
                out.push_str("Date: ");
                out.push_str(&dt.format("%Y-%m-%d").to_string());
                out.push('\n');
            }
        }
        if !self.body.is_empty() {
            out.push('\n');
            out.push_str(&self.body);
        }
        // A message with no subject, no sender, no date and no indexed body
        // would render as an empty string, which a cross-encoder scores as
        // "no evidence either way" and Claude has nothing to reason about.
        // Naming the id is not an option (the backends never see one — see
        // `claude`'s own docs on positional labels), so the fallback is a
        // marker that at least keeps the pair well-formed.
        if out.is_empty() {
            out.push_str("(no indexed content)");
        }
        out
    }
}

/// Reads rerank documents out of the message store.
#[derive(Debug, Clone)]
pub(super) struct DocumentSource {
    db: Database,
}

impl DocumentSource {
    pub(super) const fn new(db: Database) -> Self {
        Self { db }
    }

    /// Documents for `ids`, keyed by `messages.id`. Missing keys mean the
    /// row was not found, the read failed, or the query was superseded —
    /// all three are the caller's cue to leave the L1 order alone rather
    /// than rerank a partial window.
    pub(super) async fn fetch(
        &self,
        ids: &[i64],
        cancel: &CancellationToken,
    ) -> BTreeMap<i64, Document> {
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let capped = &ids[..ids.len().min(MAX_FETCH)];
        let placeholders = super::placeholder_list(capped.len());
        // `part = 'body'` mirrors `present::Presenter::fetch_meta`: the body
        // part is the one every message has, and joining every part would
        // multiply rows per message for no gain at this window size.
        // `a.name`/`mb.name` are joined for the AI policy gate, not for the
        // prompt: `ai.policy` and `accounts.ai.enabled` are keyed on those
        // names, and a rerank that could not name the account a candidate
        // came from could not honor either.
        let sql = format!(
            "SELECT m.id, m.subject, m.from_name, m.from_addr, m.date, \
                    SUBSTR(ic.text, 1, {MAX_BODY_CHARS}), a.name, mb.name \
             FROM messages m \
             LEFT JOIN index_content ic ON ic.message_id = m.id AND ic.part = 'body' \
             LEFT JOIN accounts a ON a.id = m.account_id \
             LEFT JOIN mailboxes mb ON mb.id = m.mailbox_id \
             WHERE m.id IN ({placeholders})"
        );
        let ids_owned = capped.to_vec();
        let result = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = ids_owned
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;

        match result {
            Ok(Some(rows)) => rows
                .into_iter()
                .map(
                    |(id, subject, from_name, from_addr, date, body, account, mailbox)| {
                        (
                            id,
                            Document {
                                subject: subject.unwrap_or_default(),
                                sender: sender_line(from_name.as_deref(), from_addr.as_deref()),
                                date,
                                body: body.unwrap_or_default(),
                                account: account.unwrap_or_default(),
                                mailbox: mailbox.unwrap_or_default(),
                            },
                        )
                    },
                )
                .collect(),
            Ok(None) => {
                tracing::debug!("rerank document fetch cancelled; keeping the L1 order");
                BTreeMap::new()
            }
            Err(error) => {
                tracing::warn!(%error, "rerank document fetch failed; keeping the L1 order");
                BTreeMap::new()
            }
        }
    }
}

/// `Name <addr>` when both are present, whichever one is when only one is,
/// empty when neither is.
fn sender_line(name: Option<&str>, addr: Option<&str>) -> String {
    let name = name.unwrap_or_default().trim();
    let addr = addr.unwrap_or_default().trim();
    match (name.is_empty(), addr.is_empty()) {
        (false, false) => format!("{name} <{addr}>"),
        (false, true) => name.to_owned(),
        (true, false) => addr.to_owned(),
        (true, true) => String::new(),
    }
}
