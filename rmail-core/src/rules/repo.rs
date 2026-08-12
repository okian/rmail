//! Storage for the rules engine: the rule rows, the `claude_is`
//! classification cache, the few-shot correction set, and the at-most-once
//! action claim.
//!
//! Every function here is `async` and goes through [`Database::read`]/
//! [`Database::write`], so `rusqlite` work runs on the blocking pool and
//! never on a Tokio worker — the crate-wide rule, restated because this
//! module is the only place in `rules` that touches SQL at all. Keeping it
//! all here is what lets [`super::eval`] be a pure function of a snapshot.

use rusqlite::OptionalExtension;

use crate::error::Error;
use crate::rules::classify::Example;
use crate::storage::Database;

/// One persisted rule row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRule {
    /// Row id.
    pub id: i64,
    /// Owning account.
    pub account_id: i64,
    /// Unique (per account, case-insensitively) name.
    pub name: String,
    /// The rule document, verbatim.
    pub toml: String,
    /// Whether the evaluator fires it.
    pub enabled: bool,
    /// Creation time, unix seconds.
    pub created_at: i64,
    /// Last change, unix seconds.
    pub updated_at: i64,
}

/// A cached `claude_is` verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedClassification {
    /// The verdict.
    pub verdict: bool,
    /// The model's one-line justification.
    pub explanation: String,
    /// The model that produced it.
    pub model: String,
}

/// The few-shot context for one (account, predicate, message).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FewShot {
    /// Corrections recorded for *other* messages, oldest first — replayed as
    /// prior turns.
    pub examples: Vec<Example>,
    /// A correction recorded for *this* message, if any. Authoritative: the
    /// user has already answered this exact question, so there is nothing to
    /// ask a model.
    pub correction: Option<bool>,
}

const RULE_COLS: &str = "id, account_id, name, toml, enabled, created_at, updated_at";

fn stored_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredRule> {
    Ok(StoredRule {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        toml: row.get(3)?,
        enabled: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

/// What [`insert_rule`] resolved to. A three-way outcome rather than an
/// `Error` from inside the write closure, for the reason
/// `smart_folder::SmartFolderStore::create` gives: the closure's error type is
/// `rusqlite`'s, and mapping a constraint violation to a domain error inside
/// it would mean stringly-typing the distinction back out.
enum InsertOutcome {
    Created(StoredRule),
    Duplicate,
    NoAccount,
}

/// Insert a rule.
///
/// # Errors
/// [`Error::AlreadyExists`] if the account already has a rule by that name;
/// [`Error::NotFound`] if `account_id` names no account; otherwise a mapped
/// storage error.
pub async fn insert_rule(
    db: &Database,
    account_id: i64,
    name: &str,
    toml: &str,
    enabled: bool,
) -> Result<StoredRule, Error> {
    let name_owned = name.to_owned();
    let toml_owned = toml.to_owned();
    let for_error = name.to_owned();
    let outcome = db
        .write(move |conn| {
            let inserted = conn.query_row(
                &format!(
                    "INSERT INTO rules (account_id, name, toml, enabled)
                     VALUES (?1, ?2, ?3, ?4)
                     RETURNING {RULE_COLS}"
                ),
                rusqlite::params![account_id, name_owned, toml_owned, i64::from(enabled)],
                stored_rule,
            );
            match inserted {
                Ok(rule) => Ok(InsertOutcome::Created(rule)),
                Err(err) if crate::saved_search::repo::is_unique_violation(&err) => {
                    Ok(InsertOutcome::Duplicate)
                }
                Err(err) if crate::saved_search::repo::is_missing_reference(&err) => {
                    Ok(InsertOutcome::NoAccount)
                }
                Err(err) => Err(err),
            }
        })
        .await?;

    match outcome {
        InsertOutcome::Created(rule) => Ok(rule),
        InsertOutcome::Duplicate => Err(Error::already_exists(format!(
            "a rule named {for_error:?} already exists in this account"
        ))),
        InsertOutcome::NoAccount => Err(Error::not_found(format!("account {account_id}"))),
    }
}

/// One account's rules, alphabetical by name.
///
/// # Errors
/// A mapped storage error.
pub async fn list_rules(db: &Database, account_id: i64) -> Result<Vec<StoredRule>, Error> {
    Ok(db
        .read(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {RULE_COLS} FROM rules WHERE account_id = ?1 ORDER BY name"
            ))?;
            let rules = stmt
                .query_map([account_id], stored_rule)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rules)
        })
        .await?)
}

/// One rule by name.
///
/// # Errors
/// [`Error::NotFound`] if the account has no rule by that name; otherwise a
/// mapped storage error.
pub async fn get_rule(db: &Database, account_id: i64, name: &str) -> Result<StoredRule, Error> {
    let name_owned = name.trim().to_owned();
    let for_error = name_owned.clone();
    db.read(move |conn| {
        conn.query_row(
            &format!("SELECT {RULE_COLS} FROM rules WHERE account_id = ?1 AND name = ?2"),
            rusqlite::params![account_id, name_owned],
            stored_rule,
        )
        .optional()
    })
    .await?
    .ok_or_else(|| {
        Error::not_found(format!(
            "no rule named {for_error:?} in account {account_id}"
        ))
    })
}

/// An account's display name, for policy resolution.
///
/// # Errors
/// A mapped storage error.
pub async fn account_name(db: &Database, account_id: i64) -> Result<Option<String>, Error> {
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT name FROM accounts WHERE id = ?1",
                [account_id],
                |r| r.get::<_, String>(0),
            )
            .optional()
        })
        .await?)
}

/// The cached verdict for `message_id` under `prompt_hash`, if any.
///
/// # Errors
/// A mapped storage error.
pub async fn cached_classification(
    db: &Database,
    message_id: i64,
    prompt_hash: &str,
) -> Result<Option<CachedClassification>, Error> {
    let hash = prompt_hash.to_owned();
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT verdict, explanation, model FROM rule_classifications
                 WHERE message_id = ?1 AND prompt_hash = ?2",
                rusqlite::params![message_id, hash],
                |row| {
                    Ok(CachedClassification {
                        verdict: row.get::<_, i64>(0)? != 0,
                        explanation: row.get(1)?,
                        model: row.get(2)?,
                    })
                },
            )
            .optional()
        })
        .await?)
}

/// Cache one verdict.
///
/// Upserts on `(message_id, prompt_hash)` — re-classifying the same message
/// under the same prompt (which only happens after the row was pruned with
/// its message, or when two evaluations raced) replaces its own entry rather
/// than failing the call that produced a perfectly good answer.
///
/// # Errors
/// A mapped storage error.
pub async fn cache_classification(
    db: &Database,
    message_id: i64,
    prompt_hash: &str,
    verdict: bool,
    explanation: &str,
    model: &str,
    ledger_entry_id: Option<i64>,
) -> Result<(), Error> {
    let hash = prompt_hash.to_owned();
    let explanation = explanation.to_owned();
    let model = model.to_owned();
    db.write(move |conn| {
        conn.execute(
            "INSERT INTO rule_classifications
                 (message_id, prompt_hash, verdict, explanation, model, ledger_entry_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(message_id, prompt_hash) DO UPDATE SET
                 verdict = excluded.verdict,
                 explanation = excluded.explanation,
                 model = excluded.model,
                 ledger_entry_id = excluded.ledger_entry_id,
                 created_at = unixepoch()",
            rusqlite::params![
                message_id,
                hash,
                i64::from(verdict),
                explanation,
                model,
                ledger_entry_id
            ],
        )
    })
    .await?;
    Ok(())
}

/// The few-shot context for one classification.
///
/// `limit` bounds how many corrections are replayed — every example is
/// tokens on every uncached call, so an unbounded set is unbounded spend.
/// The most recent are kept (a user's latest corrections describe what they
/// currently mean) but replayed oldest-first, which is both the natural
/// reading order and what makes the ordering — and therefore the prompt hash
/// — deterministic.
///
/// # Errors
/// A mapped storage error.
pub async fn few_shot(
    db: &Database,
    account_id: i64,
    prompt: &str,
    limit: usize,
    message_id: i64,
) -> Result<FewShot, Error> {
    let prompt = prompt.trim().to_owned();
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    Ok(db
        .read(move |conn| {
            let correction: Option<bool> = conn
                .query_row(
                    "SELECT expected FROM rule_examples
                     WHERE account_id = ?1 AND prompt = ?2 AND message_id = ?3",
                    rusqlite::params![account_id, prompt, message_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .map(|v| v != 0);

            let mut stmt = conn.prepare(
                "SELECT rendered, expected FROM rule_examples
                 WHERE account_id = ?1 AND prompt = ?2
                   AND (message_id IS NULL OR message_id <> ?3)
                 ORDER BY id DESC
                 LIMIT ?4",
            )?;
            let mut examples = stmt
                .query_map(
                    rusqlite::params![account_id, prompt, message_id, limit],
                    |row| {
                        Ok(Example {
                            rendered: row.get(0)?,
                            expected: row.get::<_, i64>(1)? != 0,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            examples.reverse();
            Ok(FewShot {
                examples,
                correction,
            })
        })
        .await?)
}

/// Record a user correction, which becomes a few-shot example.
///
/// Upserts on `(account_id, prompt, message_id)`: correcting the same message
/// twice replaces the earlier verdict rather than teaching both answers.
///
/// # Errors
/// [`Error::NotFound`] if `account_id` names no account; otherwise a mapped
/// storage error.
pub async fn record_example(
    db: &Database,
    account_id: i64,
    prompt: &str,
    message_id: i64,
    rendered: &str,
    expected: bool,
) -> Result<(), Error> {
    let prompt = prompt.trim().to_owned();
    let rendered = rendered.to_owned();
    let missing = db
        .write(move |conn| {
            let result = conn.execute(
                "INSERT INTO rule_examples
                     (account_id, prompt, message_id, rendered, expected)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(account_id, prompt, message_id) DO UPDATE SET
                     rendered = excluded.rendered,
                     expected = excluded.expected,
                     created_at = unixepoch()",
                rusqlite::params![
                    account_id,
                    prompt,
                    message_id,
                    rendered,
                    i64::from(expected)
                ],
            );
            match result {
                Ok(_) => Ok(false),
                Err(err) if crate::saved_search::repo::is_missing_reference(&err) => Ok(true),
                Err(err) => Err(err),
            }
        })
        .await?;
    if missing {
        return Err(Error::not_found(format!(
            "account {account_id} or message {message_id}"
        )));
    }
    Ok(())
}

/// How many corrections exist for one predicate — reported by
/// `RecordCorrection` so a caller can see the few-shot set growing.
///
/// # Errors
/// A mapped storage error.
pub async fn example_count(db: &Database, account_id: i64, prompt: &str) -> Result<i64, Error> {
    let prompt = prompt.trim().to_owned();
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM rule_examples WHERE account_id = ?1 AND prompt = ?2",
                rusqlite::params![account_id, prompt],
                |row| row.get::<_, i64>(0),
            )
        })
        .await?)
}

/// Claim `(rule_id, message_id)` for action.
///
/// Returns `true` when this caller won the claim and must run the actions,
/// `false` when the rule has already acted on this message. The insert is the
/// claim — there is no read-then-write window for a second evaluator to slip
/// through, which is what makes this safe against the background evaluator
/// and an `EvaluateRules` RPC running at the same moment.
///
/// # Errors
/// A mapped storage error. A rule or message deleted concurrently surfaces as
/// a foreign-key violation, reported as `false` (nothing to act on) rather
/// than as an error: losing a claim to a deletion is exactly the same outcome
/// as losing it to another evaluator.
pub async fn claim(db: &Database, rule_id: i64, message_id: i64) -> Result<bool, Error> {
    Ok(db
        .write(move |conn| {
            let result = conn.execute(
                "INSERT OR IGNORE INTO rule_actions_fired (rule_id, message_id) VALUES (?1, ?2)",
                rusqlite::params![rule_id, message_id],
            );
            match result {
                Ok(rows) => Ok(rows > 0),
                Err(err) if crate::saved_search::repo::is_missing_reference(&err) => Ok(false),
                Err(err) => Err(err),
            }
        })
        .await?)
}

/// Whether `(rule_id, message_id)` has already been acted on. Read-only —
/// the dry-run path's way of reporting "this would have fired, but it already
/// did" without claiming anything.
///
/// # Errors
/// A mapped storage error.
pub async fn already_fired(db: &Database, rule_id: i64, message_id: i64) -> Result<bool, Error> {
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT 1 FROM rule_actions_fired WHERE rule_id = ?1 AND message_id = ?2",
                rusqlite::params![rule_id, message_id],
                |_| Ok(()),
            )
            .optional()
        })
        .await?
        .is_some())
}

/// Resolve a mailbox by name within an account.
///
/// # Errors
/// A mapped storage error.
pub async fn mailbox_id(db: &Database, account_id: i64, name: &str) -> Result<Option<i64>, Error> {
    let name = name.to_owned();
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT id FROM mailboxes WHERE account_id = ?1 AND name = ?2",
                rusqlite::params![account_id, name],
                |row| row.get::<_, i64>(0),
            )
            .optional()
        })
        .await?)
}

/// The account's configured username, used as the `From` identity of a
/// `draft_reply`.
///
/// # Errors
/// A mapped storage error.
pub async fn account_username(db: &Database, account_id: i64) -> Result<Option<String>, Error> {
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT username FROM accounts WHERE id = ?1",
                [account_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
        })
        .await?
        .flatten())
}
