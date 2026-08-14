//! Persistence for the shield: recording what a message tried, reading it
//! back, and the confirmation that releases a withheld action.
//!
//! Split out of [`super`] because that module is deliberately pure — [`super::scan`]
//! takes text and returns findings with no database, no configuration and no
//! I/O, which is what lets it be called from inside a request-building path
//! and tested without a fixture. Everything that touches `ai_injection_flags`
//! (V36) lives here instead.
//!
//! # Recording never fails a request
//!
//! [`record`] is the wrapper every AI pass uses, and it swallows its own
//! errors after logging them. A failed write to this table means the shield
//! lost some *observability* for one message; turning that into a failed
//! triage call would mean a full disk stops the mailbox being summarized,
//! which is a strictly worse outcome. The one place that must not swallow is
//! the rules action gate, which reads through [`get`] and propagates —
//! there, not knowing whether a message is flagged is exactly the case that
//! has to fail closed rather than proceed.

use rusqlite::OptionalExtension;

use super::{Detection, InjectionKind, ScanReport, Severity};
use crate::ai::queue::assemble_content;
use crate::config::{AiInjection, AiPrivacy};
use crate::error::Error;
use crate::storage::Database;

/// A message's stored flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    /// The flagged message.
    pub message_id: i64,
    /// Its owning account.
    pub account_id: i64,
    /// The highest severity found.
    pub severity: Severity,
    /// What was found, as written.
    pub detections: Vec<Detection>,
    /// When the scan that produced this ran (unix seconds).
    pub scanned_at: i64,
    /// When a human confirmed that AI-decided actions may act on this
    /// message anyway, or `None` — the only thing that releases a withheld
    /// action.
    pub confirmed_at: Option<i64>,
}

impl Flag {
    /// The distinct kinds found, ascending.
    #[must_use]
    pub fn kinds(&self) -> Vec<InjectionKind> {
        let mut kinds: Vec<InjectionKind> = self.detections.iter().map(|d| d.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        kinds
    }

    /// Whether a human has confirmed this message.
    #[must_use]
    pub fn is_confirmed(&self) -> bool {
        self.confirmed_at.is_some()
    }

    /// Whether the shield is, right now, withholding AI-decided actions on
    /// this message under `config`.
    ///
    /// The one place this comparison is written, so a UI, an RPC response
    /// and [`crate::rules::RuleEngine`] cannot disagree about whether a
    /// message is gated.
    #[must_use]
    pub fn withholds_actions(&self, config: &AiInjection) -> bool {
        !self.is_confirmed() && super::blocks_actions(Some(self.severity), config)
    }
}

/// The JSON shape one detection is stored as. A private mirror of
/// [`Detection`] rather than `serde` derives on the public type: the column
/// is a storage detail this module owns, and deriving `Serialize` on
/// [`Detection`] would make its field names part of every caller's contract.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredDetection {
    kind: String,
    excerpt: String,
    offset: usize,
}

/// Write (or replace) `message_id`'s flag from `report`, and return what was
/// stored.
///
/// A clean report **removes** any existing row rather than storing an empty
/// one: the table's contract is "a row means flagged" (see V36), and a
/// message whose body was edited or re-extracted into something harmless
/// must stop being gated.
///
/// # A confirmation survives an identical re-scan and not a different one
///
/// Re-scanning the same unchanged text produces the same findings, and
/// clearing the confirmation each time would mean the user is asked again
/// after every triage pass — a prompt they would learn to click through,
/// which is worse than not asking. Findings that *differ* clear it: consent
/// was given to what the message said then, and this is not that.
///
/// # Errors
/// A mapped storage error. Callers on a request path should prefer
/// [`record`], which logs and swallows.
pub async fn flag(
    db: &Database,
    message_id: i64,
    account_id: i64,
    report: &ScanReport,
) -> Result<Option<Flag>, Error> {
    let Some(severity) = report.severity() else {
        db.write(move |conn| {
            conn.execute(
                "DELETE FROM ai_injection_flags WHERE message_id = ?1",
                [message_id],
            )
        })
        .await?;
        return Ok(None);
    };

    let kinds = serde_json::to_string(
        &report
            .kinds()
            .into_iter()
            .map(InjectionKind::as_str)
            .collect::<Vec<_>>(),
    )
    .map_err(|e| Error::internal(format!("could not encode injection kinds: {e}")))?;
    let detections = serde_json::to_string(
        &report
            .detections
            .iter()
            .map(|d| StoredDetection {
                kind: d.kind.as_str().to_owned(),
                excerpt: d.excerpt.clone(),
                offset: d.offset,
            })
            .collect::<Vec<_>>(),
    )
    .map_err(|e| Error::internal(format!("could not encode injection detections: {e}")))?;
    let severity_wire = severity.as_str().to_owned();

    db.write(move |conn| {
        conn.execute(
            "INSERT INTO ai_injection_flags (
                 message_id, account_id, severity, kinds, detections, scanned_at, confirmed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, unixepoch(), NULL)
             ON CONFLICT(message_id) DO UPDATE SET
                 account_id = excluded.account_id,
                 severity = excluded.severity,
                 kinds = excluded.kinds,
                 detections = excluded.detections,
                 scanned_at = excluded.scanned_at,
                 -- See this function's docs: consent is to a specific set of
                 -- findings, so it survives an identical re-scan and only
                 -- that. Compared on the detection list rather than on
                 -- severity, which is far too coarse -- a message that
                 -- swapped one hostile phrasing for another would otherwise
                 -- keep a confirmation given for the first.
                 confirmed_at = CASE
                     WHEN ai_injection_flags.detections = excluded.detections
                     THEN ai_injection_flags.confirmed_at
                     ELSE NULL
                 END",
            rusqlite::params![
                message_id,
                account_id,
                severity_wire,
                kinds,
                detections,
                // `scanned_at`/`confirmed_at` are set by the SQL above.
            ],
        )
    })
    .await?;
    get(db, message_id).await
}

/// [`flag`], for a caller on a request path that must not fail because the
/// shield could not write a row — see the module docs.
pub async fn record(db: &Database, message_id: i64, account_id: i64, report: &ScanReport) {
    if let Some(severity) = report.severity() {
        tracing::warn!(
            message_id,
            account_id,
            severity = severity.as_str(),
            kinds = ?report.kinds().into_iter().map(InjectionKind::as_str).collect::<Vec<_>>(),
            detections = report.detections.len(),
            "prompt-injection signals in message content sent to a model"
        );
    }
    if let Err(error) = flag(db, message_id, account_id, report).await {
        tracing::warn!(
            %error,
            message_id,
            "could not record a prompt-injection scan; the message is still fenced but is \
             not flagged for the action gate"
        );
    }
}

/// Scan one message the way the AI pipeline sees it, record the result, and
/// return the flag (or `None` when it is clean).
///
/// This is `AiSafetyService.ScanInjection`'s whole implementation, and it
/// makes no model call — the detector is a local pattern scan over text this
/// daemon already holds.
///
/// # Two texts, because they hide different things
///
/// The scan covers [`crate::ai::triage::render_user_message`]'s output over
/// [`assemble_content`] — literally the bytes a triage request would carry,
/// so a finding is never about text no pass would have sent — **and**, when
/// the message has one, its raw `body_html`. The second is not redundant:
/// `assemble_content` falls back to [`crate::index::extract::strip_html`],
/// which turns a `display:none` paragraph into ordinary visible plain text.
/// By the time a prompt exists, the hiding is gone and only its payload
/// remains, so the class of attack that hides from the *human* rather than
/// from the model can only be seen in the markup. Detections from both are
/// merged into one report.
///
/// # Errors
/// [`Error::NotFound`] if the message no longer exists; otherwise a mapped
/// storage error.
#[tracing::instrument(skip(db, privacy, config), fields(message_id, severity))]
pub async fn scan_message(
    db: &Database,
    message_id: i64,
    privacy: &AiPrivacy,
    config: &AiInjection,
) -> Result<Option<Flag>, Error> {
    let content = assemble_content(db, message_id, privacy).await?;
    let account_id = content.account_id;
    let mut report =
        super::scan_if_enabled(&crate::ai::triage::render_user_message(&content), config);

    if config.enabled {
        let html: Option<String> = db
            .read(move |conn| {
                conn.query_row(
                    "SELECT body_html FROM messages WHERE id = ?1",
                    [message_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
            })
            .await?
            .flatten();
        if let Some(html) = html {
            // Offsets from the HTML scan index into the *markup*, not into
            // the assembled text — two different coordinate spaces sharing
            // one report. That is acceptable because `offset` is a hint for
            // highlighting, never an identity: `excerpt` is what a user
            // reads and what makes a detection recognizable, and merging is
            // what keeps "this message tried something" a single answer
            // rather than two the caller has to reconcile.
            report.detections.extend(super::scan(&html).detections);
            report.detections.truncate(super::MAX_DETECTIONS);
        }
    }

    if let Some(severity) = report.severity() {
        tracing::Span::current().record("severity", severity.as_str());
    }
    flag(db, message_id, account_id, &report).await
}

/// One message's flag, or `None` if it has none.
///
/// # Errors
/// A mapped storage error, or [`Error::Internal`] if a stored row cannot be
/// decoded — a row this build wrote in a shape it cannot read is a bug worth
/// surfacing, not one worth silently treating as "unflagged" on a path whose
/// whole job is to fail closed.
pub async fn get(db: &Database, message_id: i64) -> Result<Option<Flag>, Error> {
    let row = db
        .read(move |conn| {
            conn.query_row(
                "SELECT account_id, severity, detections, scanned_at, confirmed_at
                 FROM ai_injection_flags WHERE message_id = ?1",
                [message_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                },
            )
            .optional()
        })
        .await?;
    let Some((account_id, severity, detections, scanned_at, confirmed_at)) = row else {
        return Ok(None);
    };
    let severity = Severity::parse(&severity).ok_or_else(|| {
        Error::internal(format!(
            "ai_injection_flags row for message {message_id} holds an unknown severity \
             {severity:?}"
        ))
    })?;
    let stored: Vec<StoredDetection> = serde_json::from_str(&detections).map_err(|e| {
        Error::internal(format!(
            "ai_injection_flags row for message {message_id} holds undecodable detections: {e}"
        ))
    })?;
    let detections = stored
        .into_iter()
        .map(|d| {
            let kind = InjectionKind::parse(&d.kind).ok_or_else(|| {
                Error::internal(format!(
                    "ai_injection_flags row for message {message_id} names an unknown kind {:?}",
                    d.kind
                ))
            })?;
            Ok(Detection {
                kind,
                excerpt: d.excerpt,
                offset: d.offset,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(Some(Flag {
        message_id,
        account_id,
        severity,
        detections,
        scanned_at,
        confirmed_at,
    }))
}

/// Record (or withdraw) a human's confirmation that AI-decided actions may
/// act on `message_id` despite its flag.
///
/// Returns the flag as it now stands, or [`Error::NotFound`] when the
/// message has no flag: confirming a message nothing objected to is a
/// no-op the caller almost certainly did not mean, and silently succeeding
/// would let a client believe it had pre-approved a message it had not.
///
/// # Errors
/// [`Error::NotFound`] if `message_id` has no flag; otherwise a mapped
/// storage error.
pub async fn set_confirmed(db: &Database, message_id: i64, confirmed: bool) -> Result<Flag, Error> {
    let updated = db
        .write(move |conn| {
            conn.execute(
                "UPDATE ai_injection_flags
                 SET confirmed_at = CASE WHEN ?2 THEN unixepoch() ELSE NULL END
                 WHERE message_id = ?1",
                rusqlite::params![message_id, confirmed],
            )
        })
        .await?;
    if updated == 0 {
        return Err(Error::not_found(format!(
            "message {message_id} has no prompt-injection flag to confirm"
        )));
    }
    tracing::info!(
        message_id,
        confirmed,
        "prompt-injection confirmation changed"
    );
    get(db, message_id).await?.ok_or_else(|| {
        // The row was deleted between the UPDATE and the re-read (a message
        // expunged mid-call). Reported as NotFound rather than Internal:
        // from the caller's side that is exactly what happened.
        Error::not_found(format!(
            "message {message_id}'s prompt-injection flag disappeared while being confirmed"
        ))
    })
}
