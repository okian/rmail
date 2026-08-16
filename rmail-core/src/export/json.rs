//! The JSON export format, and the AI-artifact join behind `--with-ai`.
//!
//! # One document, written incrementally
//!
//! The output is a single JSON object — `{"version":1,"messages":[ … ]}` —
//! not newline-delimited records, because prd.md calls the format "JSON" and
//! a caller piping it into `jq '.messages'` should not first have to know it
//! is really NDJSON. It is nevertheless *streamed*: [`JsonFramer::prologue`]
//! opens the array, each message is serialized on its own and separated with
//! a comma, and [`JsonFramer::epilogue`] closes it. Nothing ever holds more
//! than one record, so a mailbox-sized export has a message-sized heap.
//!
//! # A hand-written schema, on purpose
//!
//! [`Record`] names every field explicitly instead of deriving `Serialize` on
//! [`repo::Message`]. Same reasoning as `rmail-cli::search_cli`'s `--json`
//! contract: renaming a struct field is a source-compatible refactor that
//! would silently reshape everybody's downstream `jq`. Naming the keys once,
//! here, is what makes this a contract rather than an accident.
//!
//! # `raw_rfc822_base64` is the archive
//!
//! Every other field is a convenience projection of bytes that are already in
//! that one. It is `null` — never absent, never fabricated — for a row whose
//! raw was not stored, so a consumer can tell "this message's original bytes
//! are gone" from "this export does not carry raw bytes".
//!
//! # `--with-ai` reads; it never calls a model
//!
//! [`load_artifacts`] joins `ai_summaries` and the applied-tag view for a
//! whole page in two statements. It is a read of what tasks 48/49/55 already
//! wrote. Nothing in this module can spend money, and nothing here needs the
//! prompt-injection fence, because no text from this module is ever sent
//! anywhere.

use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::repo;
use crate::retrieve::cancel::interruptible_read;
use crate::storage::Database;

use super::LoadedMessage;

/// The schema version stamped on every export document.
///
/// Bumped when a field changes meaning or disappears — adding a field does
/// not, since a consumer reading by key is unaffected.
pub const SCHEMA_VERSION: u32 = 1;

/// Serializes messages into one streamed JSON document.
#[derive(Debug, Default)]
pub struct JsonFramer {
    /// Whether a record has already been written, so the *separator* comma
    /// goes before the second record and not after the last one — the one
    /// piece of state a streamed JSON array needs.
    wrote_any: bool,
}

impl JsonFramer {
    /// The bytes that open the document.
    #[must_use]
    pub fn prologue(&self) -> Vec<u8> {
        format!("{{\"version\":{SCHEMA_VERSION},\"messages\":[").into_bytes()
    }

    /// Serialize one message, prefixed by a separator if it is not the first.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if serialization fails — which, for a tree of
    /// owned strings and integers with no map keys but `&'static str`, means
    /// an allocation failure rather than a data problem.
    pub fn frame(&mut self, loaded: &LoadedMessage) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        if self.wrote_any {
            out.push(b',');
        }
        self.wrote_any = true;
        let record = Record::new(loaded);
        serde_json::to_writer(&mut out, &record)
            .map_err(|e| Error::internal(format!("serializing export record: {e}")))?;
        Ok(out)
    }

    /// The bytes that close the document.
    #[must_use]
    pub fn epilogue(&self) -> Vec<u8> {
        b"]}".to_vec()
    }
}

/// One message's JSON record. See the module docs on why the keys are spelled
/// out rather than derived.
#[derive(Debug, Serialize)]
struct Record<'a> {
    id: i64,
    account_id: i64,
    mailbox_id: i64,
    mailbox: Option<&'a str>,
    uid: i64,
    uidvalidity: i64,
    message_id: Option<&'a str>,
    thread_id: Option<i64>,
    in_reply_to: Option<&'a str>,
    references: Option<&'a str>,
    subject: Option<&'a str>,
    from: Option<Address<'a>>,
    to: Vec<&'a str>,
    cc: Vec<&'a str>,
    /// Unix seconds from the `Date` header.
    date: Option<i64>,
    /// Unix seconds from IMAP `INTERNALDATE`.
    internaldate: Option<i64>,
    size: Option<i64>,
    flags: &'a [String],
    has_attachments: bool,
    attachments: Vec<AttachmentRecord<'a>>,
    body_text: Option<&'a str>,
    body_html: Option<&'a str>,
    /// The original RFC822 octets, base64 (standard alphabet, padded).
    raw_rfc822_base64: Option<String>,
    /// Present only under `--with-ai`; absent (not null) otherwise, so a
    /// consumer can tell an export that did not ask for AI from a message
    /// that has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    ai: Option<&'a AiArtifacts>,
}

impl<'a> Record<'a> {
    fn new(loaded: &'a LoadedMessage) -> Self {
        let message = &loaded.message;
        Self {
            id: message.id,
            account_id: message.account_id,
            mailbox_id: message.mailbox_id,
            mailbox: loaded.mailbox.as_deref(),
            uid: message.uid,
            uidvalidity: message.uidvalidity,
            message_id: message.message_id.as_deref(),
            thread_id: message.thread_id,
            in_reply_to: message.in_reply_to.as_deref(),
            references: message.references_hdr.as_deref(),
            subject: message.subject.as_deref(),
            from: Address::new(message.from_name.as_deref(), message.from_addr.as_deref()),
            to: split_addresses(message.to_addrs.as_deref()),
            cc: split_addresses(message.cc_addrs.as_deref()),
            date: message.date,
            internaldate: message.internaldate,
            size: message.size,
            flags: &loaded.flags,
            has_attachments: message.has_attachments,
            attachments: loaded
                .attachments
                .iter()
                .map(AttachmentRecord::new)
                .collect(),
            body_text: message.body_text.as_deref(),
            body_html: message.body_html.as_deref(),
            raw_rfc822_base64: message.raw.as_deref().map(|raw| BASE64.encode(raw)),
            ai: loaded.ai.as_ref(),
        }
    }
}

/// A mailbox address with its display name, if the row carried one.
#[derive(Debug, Serialize)]
struct Address<'a> {
    name: Option<&'a str>,
    address: Option<&'a str>,
}

impl<'a> Address<'a> {
    fn new(name: Option<&'a str>, address: Option<&'a str>) -> Option<Self> {
        (name.is_some() || address.is_some()).then_some(Self { name, address })
    }
}

/// One attachment's metadata. The bytes are not repeated here — they are
/// already inside `raw_rfc822_base64`, and duplicating a 20 MB attachment
/// into the same document twice would double every archive for nothing.
#[derive(Debug, Serialize)]
struct AttachmentRecord<'a> {
    part_id: Option<&'a str>,
    filename: Option<&'a str>,
    content_type: Option<&'a str>,
    size: Option<i64>,
    content_id: Option<&'a str>,
    is_inline: bool,
}

impl<'a> AttachmentRecord<'a> {
    fn new(attachment: &'a repo::Attachment) -> Self {
        Self {
            part_id: attachment.part_id.as_deref(),
            filename: attachment.filename.as_deref(),
            content_type: attachment.content_type.as_deref(),
            size: attachment.size,
            content_id: attachment.content_id.as_deref(),
            is_inline: attachment.is_inline,
        }
    }
}

/// Undo `message::parse`'s `", "` join of an address header.
///
/// Not a re-parse of RFC 5322: the column was written by joining already
/// parsed addresses, so splitting on the exact separator that wrote it is the
/// inverse. Anything more ambitious would be a second address parser
/// disagreeing with the first.
fn split_addresses(joined: Option<&str>) -> Vec<&str> {
    joined
        .map(|value| {
            value
                .split(", ")
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The stored AI artifacts for one message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AiArtifacts {
    /// Every `ai_summaries` row for the message — one per (pass, model), so
    /// a triage verdict and a deep summary appear side by side rather than
    /// one overwriting the other.
    pub summaries: Vec<AiSummary>,
    /// Applied tag names, message-level and inherited from the thread,
    /// sorted. Pending suggestions and rejected ones are excluded: they are
    /// not tags on this message, they are the tagger's working state.
    pub tags: Vec<String>,
}

/// One `ai_summaries` row, projected for export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AiSummary {
    /// `"triage"` | `"deep"` | … — the open pass vocabulary.
    pub pass: String,
    /// The model that produced it.
    pub model: String,
    /// The artifact schema the producing pass wrote under.
    pub schema_version: i64,
    /// Unix seconds.
    pub created_at: i64,
    pub tl_dr: Option<String>,
    pub summary: Option<String>,
    pub thread_summary: Option<String>,
    /// Stored as a JSON array of strings; re-parsed here so the export
    /// carries structure rather than a string containing JSON.
    pub key_points: Option<serde_json::Value>,
    pub todos: Option<serde_json::Value>,
    pub suggested_tags: Option<serde_json::Value>,
    pub sentiment: Option<String>,
    pub category: Option<String>,
    pub priority: Option<String>,
    pub needs_reply: Option<bool>,
    pub suggested_reply: Option<String>,
    /// The audit-ledger row the producing call was recorded under, so an
    /// exported summary can still be traced to what was actually sent.
    pub ledger_entry_id: Option<i64>,
}

/// Load stored AI artifacts for a whole page of messages.
///
/// Two statements for the page, not two per message — prd.md's
/// "batch-attaches". Messages with no artifacts are simply absent from the
/// map; the caller substitutes [`AiArtifacts::default`].
///
/// # Errors
///
/// [`Error::Cancelled`] if `cancel` fired, a mapped storage error otherwise.
pub async fn load_artifacts(
    db: &Database,
    ids: &[i64],
    cancel: &CancellationToken,
) -> Result<BTreeMap<i64, AiArtifacts>, Error> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let ids = ids.to_vec();
    let loaded = interruptible_read(db, cancel, move |conn| {
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();

        let mut out: BTreeMap<i64, AiArtifacts> = BTreeMap::new();

        let sql = format!(
            "SELECT message_id, pass, model, schema_version, created_at, tl_dr, summary, \
                    thread_summary, key_points, todos, suggested_tags, sentiment, category, \
                    priority, needs_reply, suggested_reply, ledger_entry_id \
             FROM ai_summaries WHERE message_id IN ({placeholders}) \
             ORDER BY message_id, pass, model"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((
                row.get::<_, i64>("message_id")?,
                AiSummary {
                    pass: row.get("pass")?,
                    model: row.get("model")?,
                    schema_version: row.get("schema_version")?,
                    created_at: row.get("created_at")?,
                    tl_dr: row.get("tl_dr")?,
                    summary: row.get("summary")?,
                    thread_summary: row.get("thread_summary")?,
                    key_points: json_column(row, "key_points")?,
                    todos: json_column(row, "todos")?,
                    suggested_tags: json_column(row, "suggested_tags")?,
                    sentiment: row.get("sentiment")?,
                    category: row.get("category")?,
                    priority: row.get("priority")?,
                    needs_reply: row.get::<_, Option<i64>>("needs_reply")?.map(|v| v != 0),
                    suggested_reply: row.get("suggested_reply")?,
                    ledger_entry_id: row.get("ledger_entry_id")?,
                },
            ))
        })?;
        for row in rows {
            let (id, summary) = row?;
            out.entry(id).or_default().summaries.push(summary);
        }

        // Only `applied` rows, and both message-level and thread-inherited
        // applications — which is exactly what the view already defines, so
        // the export and `tag:` search agree by construction.
        let sql = format!(
            "SELECT mte.message_id, t.name FROM messages_tags_effective mte \
             JOIN tags t ON t.id = mte.tag_id \
             WHERE mte.message_id IN ({placeholders}) \
             ORDER BY mte.message_id, t.name"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, name) = row?;
            out.entry(id).or_default().tags.push(name);
        }

        Ok(out)
    })
    .await?;

    loaded.ok_or_else(|| Error::cancelled("export cancelled"))
}

/// Read a TEXT column that holds JSON, keeping the literal text when it does
/// not parse.
///
/// The producing passes write these with `serde_json::to_string`, so parsing
/// is the inverse. Falling back to the raw string rather than to `null`
/// matters: an export must not delete data it failed to understand.
fn json_column(row: &rusqlite::Row<'_>, name: &str) -> rusqlite::Result<Option<serde_json::Value>> {
    let text: Option<String> = row.get(name)?;
    Ok(text.map(|text| serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))))
}
