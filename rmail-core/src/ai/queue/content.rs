//! Assembling bounded, policy-safe message content — the step
//! [`crate::ai::redact`]'s module docs are explicit does not belong to it:
//! "`strip_attachments`/`max_body_chars` are not this module's job... it
//! belongs to whatever builds the `ChatRequest` in the first place (the
//! task 47 queue)."
//!
//! [`assemble_content`] runs *before* redaction in the pipeline (see the
//! parent module's docs) and produces [`MessageContent`] — everything a
//! [`super::PassHandler`] needs to build a [`ChatRequest`](crate::ai::provider::ChatRequest),
//! already bounded so nothing built from it can exceed
//! `ai.privacy.max_body_chars` or include attachment text the operator
//! turned off.

use crate::config::AiPrivacy;
use crate::error::Error;
use crate::index::extract::strip_html;
use crate::repo;
use crate::storage::Database;

/// Bounded, policy-safe content assembled from one message, ready to be
/// folded into a [`ChatRequest`](crate::ai::provider::ChatRequest) by a
/// [`super::PassHandler`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageContent {
    /// The message this was assembled from.
    pub message_id: i64,
    /// The owning account.
    pub account_id: i64,
    /// Subject line.
    pub subject: Option<String>,
    /// From display name.
    pub from_name: Option<String>,
    /// From address.
    pub from_addr: Option<String>,
    /// The body — plain text if the message had one, else HTML stripped to
    /// text, with attachment text appended when `ai.privacy.strip_attachments`
    /// is `false` — truncated to `ai.privacy.max_body_chars` characters.
    pub body: String,
    /// Whether `body` was cut short to fit `ai.privacy.max_body_chars`. A
    /// [`super::PassHandler`] may want to tell the model the body is
    /// incomplete rather than let it draw conclusions from a body that just
    /// stops mid-sentence.
    pub truncated: bool,
    /// Whether any attachment text was folded into `body` (always `false`
    /// when `ai.privacy.strip_attachments` is `true`, and also `false` when
    /// it is `false` but the message simply has no extracted attachment
    /// text yet).
    pub attachments_included: bool,
}

/// Assemble [`MessageContent`] for `message_id`, honoring
/// `ai.privacy.strip_attachments` and `ai.privacy.max_body_chars`.
///
/// # Errors
/// [`Error::NotFound`] if the message no longer exists (a sync/AI race —
/// the row was deleted between when the job was leased and when this ran).
/// Otherwise a mapped storage error.
pub async fn assemble_content(
    db: &Database,
    message_id: i64,
    privacy: &AiPrivacy,
) -> Result<MessageContent, Error> {
    let strip_attachments = privacy.strip_attachments;
    let max_chars = privacy.max_body_chars as usize;
    let (message, attachment_texts) = db
        .read(move |conn| {
            let message = repo::get_message(conn, message_id)?;
            let attachments: Vec<String> = if strip_attachments || message.is_none() {
                Vec::new()
            } else {
                let mut stmt = conn.prepare(
                    "SELECT text FROM index_content
                     WHERE message_id = ?1 AND part LIKE 'attachment:%'
                     ORDER BY part",
                )?;
                let rows = stmt
                    .query_map([message_id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            Ok((message, attachments))
        })
        .await?;

    let Some(message) = message else {
        return Err(Error::not_found(format!(
            "message {message_id} no longer exists"
        )));
    };

    let mut body = message
        .body_text
        .filter(|text| text.trim().chars().any(char::is_alphanumeric))
        .unwrap_or_else(|| {
            message
                .body_html
                .as_deref()
                .map(strip_html)
                .unwrap_or_default()
        });

    let mut attachments_included = false;
    for text in &attachment_texts {
        if text.trim().is_empty() {
            continue;
        }
        attachments_included = true;
        body.push_str("\n\n--- attachment ---\n");
        body.push_str(text);
    }

    let total_chars = body.chars().count();
    let truncated = total_chars > max_chars;
    if truncated {
        body = body.chars().take(max_chars).collect();
    }

    Ok(MessageContent {
        message_id,
        account_id: message.account_id,
        subject: message.subject,
        from_name: message.from_name,
        from_addr: message.from_addr,
        body,
        truncated,
        attachments_included,
    })
}
