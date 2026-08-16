//! The context builder: retrieved message ids in, a bounded, policy-cleared
//! pack of source documents out.
//!
//! # The policy gate is the whole point of this module
//!
//! `AskMailbox` is the one RPC in this codebase that takes an unbounded slice
//! of the mailbox, renders it into a prompt, and posts it to a third party.
//! `ai.policy` / `accounts.ai.enabled` are the operator's statement about
//! which mail may do that, and they are keyed on `(account, folder)` — which
//! is not knowable until retrieval has named the candidates. So this is the
//! one place a *question* can honor them.
//!
//! [`pack`] resolves [`crate::ai::PolicyEngine`] for every candidate **before
//! any of its text is rendered into a source document**, and a candidate whose
//! decision does not `permits_network()` is dropped there — its subject,
//! sender and body never enter [`Packed::sources`], which is the only thing
//! the prompt is built from. `local_only` and `forbidden` are both excluded:
//! the answer leaves the host, so `is_visible()` is not the test, exactly as
//! [`crate::rank::l2`] resolves it for the network reranker.
//!
//! # Dropping, not refusing
//!
//! [`crate::rank::l2::L2Stage`] refuses the *whole* rerank when a single
//! candidate fails the gate, because omitting one candidate silently changes
//! the ranking of the others. Retrieval-augmented generation has no such
//! coupling: a source document either contributes to the answer or it does
//! not, so a forbidden folder is simply absent from the context and the answer
//! is grounded on what remained. Refusing the whole question instead would
//! make `mail ask` unusable for anybody who keeps one `local_only` folder,
//! which is the configuration this feature most has to respect.
//!
//! When *nothing* survives the gate, [`Packed::sources`] is empty and the
//! caller refuses without ever building a request — see [`super`]'s own docs.
//!
//! # The token budget stops, it does not skip
//!
//! Candidates arrive best-first. Packing stops at the first one that would
//! cross `ai.ask.max_context_tokens` rather than skipping it and trying the
//! next: continuing would fill the remaining budget with *less* relevant mail
//! purely because it happened to be shorter, which is a worse context than a
//! smaller one built strictly from the best matches.

use std::collections::BTreeMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::ai::{PolicyEngine, PolicyTarget};
use crate::config::AiAsk;
use crate::error::Error;
use crate::index::chunk::estimate_tokens;
use crate::retrieve::cancel::interruptible_read;
use crate::storage::Database;

/// Hard ceiling on how many candidates one fetch will look up, mirroring
/// [`crate::rank::l2`]'s own cap. `ai.ask.top_k` is the real bound; this
/// exists so a misconfiguration cannot turn one question into an unbounded
/// `IN (...)` list.
///
/// `pub`, because it is not merely an internal safety net for callers that
/// select their own candidate set: everything past it is *silently* absent
/// from [`Packed`] — not withheld, not dropped for budget, just never fetched
/// — so a caller that hands over more ids than this would misreport its own
/// coverage. [`crate::digest`] clamps against it for exactly that reason.
pub const MAX_FETCH: usize = 200;

/// The two bounds [`pack`] enforces.
///
/// Named separately from [`AiAsk`] because packing is not an `AskMailbox`
/// concern any more: [`crate::digest`] (task 70) builds a context the same way
/// — same policy gate, same fencing, same budget discipline — from its own
/// `[digest]` table, and threading an `AiAsk` through it would have meant
/// either a second copy of this function or a digest configured out of the
/// mailbox-RAG table. Neither is a real option, and both would have let the
/// two paths' policy handling drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackLimits {
    /// Ceiling on the assembled context, in estimated tokens.
    pub max_context_tokens: usize,
    /// Ceiling on how much of one message's body may enter it.
    pub max_chars_per_message: usize,
}

impl From<&AiAsk> for PackLimits {
    fn from(ask: &AiAsk) -> Self {
        Self {
            max_context_tokens: ask.max_context_tokens as usize,
            max_chars_per_message: ask.max_chars_per_message as usize,
        }
    }
}

/// One message as the answering model will see it: an identity the citation
/// layer maps back to, and the bounded text that entered the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// `messages.id` — the local identity a citation resolves to.
    pub message_id: i64,
    /// `messages.uid`, prd.md's `message_uid` on a citation. Only unique
    /// within `(account, mailbox, uidvalidity)`, which is why `message_id`
    /// travels beside it rather than instead of it.
    pub message_uid: i64,
    /// Owning account id.
    pub account_id: i64,
    /// Owning mailbox name, for display on a citation.
    pub mailbox: String,
    /// `messages.subject`, empty when the message has none.
    pub subject: String,
    /// `messages.from_addr`, empty when the message has none.
    pub from_addr: String,
    /// `messages.date` as unix seconds, when the row has one.
    pub date: Option<i64>,
    /// The bounded body excerpt that entered the prompt — and therefore the
    /// only text a citation quote may be drawn from.
    pub body: String,
}

impl Source {
    /// The labelled block this source contributes to the prompt.
    ///
    /// `label` is 1-based and positional; the model never sees `message_id`,
    /// for the reasons [`crate::rank::l2::claude`] documents at length (a row
    /// id is an unbounded digit run the redaction firewall may tokenize, and
    /// nothing the model can say about one is more useful than "the fourth
    /// one").
    #[must_use]
    pub fn render(&self, label: usize) -> String {
        // Everything sender-controlled — subject, address and body alike —
        // goes inside one fenced block, exactly as `rank::l2::claude` fences
        // each of its candidates. Without this, `AskMailbox` is a fourth
        // model-facing sink that `ai::injection` does not cover, and it is the
        // worst of them: its output is free prose streamed to the user and
        // presented as sourced from their own mail.
        //
        // The label stays *outside* the block. It is the one token in this
        // document the engine authored, and `cite::resolve` resolves answers
        // against it — putting it inside would let a body that reproduced the
        // block shape appear to open a new source.
        let mut inner = String::with_capacity(self.body.len() + 160);
        // Sender-controlled text has its citation markers neutralized before
        // the model ever sees it — see `cite::neutralize_markers`. Applied to
        // the subject and body but not the label above, which the engine wrote.
        if !self.subject.is_empty() {
            inner.push_str("Subject: ");
            inner.push_str(&super::cite::neutralize_markers(&self.subject));
            inner.push('\n');
        }
        if !self.from_addr.is_empty() {
            inner.push_str("From: ");
            inner.push_str(&self.from_addr);
            inner.push('\n');
        }
        if let Some(date) = self.date {
            if let Some(dt) = chrono::DateTime::from_timestamp(date, 0) {
                inner.push_str("Date: ");
                inner.push_str(&dt.format("%Y-%m-%d").to_string());
                inner.push('\n');
            }
        }
        if !self.body.is_empty() {
            inner.push('\n');
            inner.push_str(&super::cite::neutralize_markers(&self.body));
            inner.push('\n');
        }

        let mut out = String::with_capacity(inner.len() + 128);
        out.push_str(&format!("[{label}]\n"));
        out.push_str(&crate::ai::injection::untrusted_block(
            &format!("source-{label}"),
            &inner,
        ));
        out.push('\n');
        out
    }
}

/// What [`pack`] built: the sources that will be sent, and an honest account
/// of everything that was not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Packed {
    /// Best-first, already policy-cleared and budget-bounded.
    pub sources: Vec<Source>,
    /// How many candidates retrieval produced.
    pub retrieved: usize,
    /// How many were dropped because `ai.policy`/`accounts.ai.enabled` does
    /// not let their folder reach a network provider.
    pub withheld_by_policy: usize,
    /// How many were dropped because the context budget was already full.
    pub dropped_for_budget: usize,
    /// Estimated tokens across [`Packed::sources`].
    pub context_tokens: usize,
}

/// One `messages` row plus the names the policy engine is keyed on.
struct Row {
    message_id: i64,
    message_uid: i64,
    account_id: i64,
    account: String,
    mailbox: String,
    subject: String,
    from_addr: String,
    date: Option<i64>,
    body: String,
}

/// Build the context for `ids` (best first), honoring the AI policy and the
/// configured budgets.
///
/// # Errors
///
/// [`Error`] only for a failed database read. A cancelled read, a missing
/// row, and a fully-withheld candidate set are all ordinary outcomes that
/// produce a smaller (possibly empty) [`Packed`] — the caller's cue to refuse,
/// not to fail.
pub async fn pack(
    db: &Database,
    ids: &[i64],
    policy: &Arc<PolicyEngine>,
    limits: PackLimits,
    max_body_chars: usize,
    cancel: &CancellationToken,
) -> Result<Packed, Error> {
    let mut packed = Packed {
        retrieved: ids.len(),
        ..Packed::default()
    };
    if ids.is_empty() {
        return Ok(packed);
    }

    let rows = fetch(db, ids, cancel).await?;
    // One message may contribute no more than `ai.ask.max_chars_per_message`,
    // and never more than `ai.privacy.max_body_chars` — the operator's own
    // ceiling on what any single message may hand a provider, which this path
    // must not silently exceed just because it packs many messages at once.
    let per_message = limits.max_chars_per_message.min(max_body_chars);
    let budget = limits.max_context_tokens;

    // `ids`' order, not the fetch's: retrieval ranked these, and the pack has
    // to be best-first for the budget cut below to mean "the best that fit".
    let mut full = false;
    for id in ids {
        let Some(row) = rows.get(id) else {
            // Deleted between ranking and now, or never indexed. Not an
            // error — the answer is grounded on what is still there.
            continue;
        };
        // The gate, before a single character of this message's text is
        // rendered into a source. See the module docs.
        let target = PolicyTarget::account(row.account.clone()).mailbox(row.mailbox.clone());
        let decision = policy.resolve(&target);
        if !decision.permits_network() {
            packed.withheld_by_policy += 1;
            tracing::debug!(
                message_id = row.message_id,
                mode = ?decision.mode,
                "ai policy withholds this message from the ask-mailbox context"
            );
            continue;
        }
        if full {
            packed.dropped_for_budget += 1;
            continue;
        }

        let source = Source {
            message_id: row.message_id,
            message_uid: row.message_uid,
            account_id: row.account_id,
            mailbox: row.mailbox.clone(),
            subject: row.subject.clone(),
            from_addr: row.from_addr.clone(),
            date: row.date,
            body: cap_chars(&row.body, per_message),
        };
        let cost = estimate_tokens(&source.render(packed.sources.len() + 1));
        if !packed.sources.is_empty() && packed.context_tokens + cost > budget {
            // Full. Everything after this is counted, not packed — see the
            // module docs on why packing stops rather than skips.
            full = true;
            packed.dropped_for_budget += 1;
            continue;
        }
        packed.context_tokens += cost;
        packed.sources.push(source);
    }

    Ok(packed)
}

/// The rows behind `ids`, keyed by `messages.id`.
///
/// `a.name`/`mb.name` are joined for the policy gate, not for the prompt.
/// A row whose account or mailbox join found nothing yields an empty name,
/// which [`PolicyEngine::resolve`] answers for out of `ai.policy`'s defaults
/// like any other unnamed target — the same treatment [`crate::rank::l2`]
/// gives an unresolvable candidate.
async fn fetch(
    db: &Database,
    ids: &[i64],
    cancel: &CancellationToken,
) -> Result<BTreeMap<i64, Row>, Error> {
    let capped = &ids[..ids.len().min(MAX_FETCH)];
    let placeholders = placeholder_list(capped.len());
    // `part = 'body'` mirrors `rank::l2::document`'s own fetch: the body part
    // is the one every message has, and joining every part would multiply
    // rows per message for no gain at this window size. The `SUBSTR` bound is
    // generous relative to `ai.ask.max_chars_per_message` (which cuts again,
    // by character, above) and exists so a pathological message cannot pull
    // megabytes through the read pool.
    let sql = format!(
        "SELECT m.id, m.uid, m.account_id, m.subject, m.from_addr, m.date, \
                SUBSTR(ic.text, 1, {MAX_FETCH_CHARS}), a.name, mb.name \
         FROM messages m \
         LEFT JOIN index_content ic ON ic.message_id = m.id AND ic.part = 'body' \
         LEFT JOIN accounts a ON a.id = m.account_id \
         LEFT JOIN mailboxes mb ON mb.id = m.mailbox_id \
         WHERE m.id IN ({placeholders})"
    );
    let ids_owned = capped.to_vec();
    let rows = interruptible_read(db, cancel, move |conn| {
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = ids_owned
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok(Row {
                message_id: row.get(0)?,
                message_uid: row.get(1)?,
                account_id: row.get(2)?,
                subject: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                from_addr: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                date: row.get(5)?,
                body: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                account: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                mailbox: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })
    .await?;

    // A cancelled scan is an empty context, which the caller turns into a
    // refusal rather than an answer grounded on half a mailbox.
    let Some(rows) = rows else {
        tracing::debug!("ask-mailbox context fetch was cancelled");
        return Ok(BTreeMap::new());
    };
    Ok(rows.into_iter().map(|row| (row.message_id, row)).collect())
}

/// How much of one message's indexed body one fetch will read. Bounded here
/// as well as by `ai.ask.max_chars_per_message` because this is the bound on
/// what crosses the process boundary out of SQLite; the character cut above
/// is the bound on what enters a prompt.
const MAX_FETCH_CHARS: usize = 16_384;

/// `text`, cut to at most `max` characters.
///
/// By `char`, not by byte: slicing a UTF-8 string at an arbitrary byte offset
/// panics, and mail is full of multi-byte text.
fn cap_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => text.get(..cut).unwrap_or_default().to_owned(),
        None => text.to_owned(),
    }
}

/// `?, ?, ?` for an `IN (...)` clause of `n` bound parameters. Duplicated
/// from [`crate::rank::l2`]'s identical private helper for the reason that
/// module's own comment gives: it is three lines, and exporting it would make
/// a formatting detail of one module's SQL part of another's public surface.
fn placeholder_list(n: usize) -> String {
    let mut out = String::with_capacity(n * 3);
    for i in 0..n {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('?');
    }
    out
}
