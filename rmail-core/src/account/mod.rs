//! Account model and CRUD.
//!
//! A domain [`Account`] pairs the stored metadata with its
//! [`CredentialSource`] (how to resolve the password), never the password
//! itself. CRUD runs against the `accounts` table via the async [`Database`]
//! accessors and maps storage errors into the domain error model
//! ([`Error`]): a duplicate name is `ALREADY_EXISTS`, a missing account is
//! `NOT_FOUND`, invalid input is `INVALID_ARGUMENT`.

use crate::credential::CredentialSource;
use crate::error::{Error, Result};
use crate::storage::{Database, StorageError};
use crate::{repo, ErrorReason};

/// A configured account with its credential source (never the secret).
#[derive(Debug, Clone)]
pub struct Account {
    /// Stable id.
    pub id: i64,
    /// Unique account name.
    pub name: String,
    /// IMAP server hostname.
    pub imap_server: Option<String>,
    /// IMAP port.
    pub imap_port: Option<u16>,
    /// Login username.
    pub username: Option<String>,
    /// SMTP server hostname.
    pub smtp_server: Option<String>,
    /// SMTP port.
    pub smtp_port: Option<u16>,
    /// How to resolve the password (never the password).
    pub credential: CredentialSource,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last-update time (unix seconds).
    pub updated_at: i64,
}

impl Account {
    fn from_repo(row: repo::Account) -> Result<Self> {
        let credential =
            CredentialSource::from_stored(&row.secret_kind, row.secret_ref.as_deref())?;
        Ok(Self {
            id: row.id,
            name: row.name,
            imap_server: row.imap_server,
            imap_port: row.imap_port,
            username: row.username,
            smtp_server: row.smtp_server,
            smtp_port: row.smtp_port,
            credential,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

/// Fields for creating an account.
#[derive(Debug, Clone, Default)]
pub struct NewAccount {
    /// Unique account name (required, non-empty).
    pub name: String,
    /// IMAP server hostname.
    pub imap_server: Option<String>,
    /// IMAP port.
    pub imap_port: Option<u16>,
    /// Login username.
    pub username: Option<String>,
    /// SMTP server hostname.
    pub smtp_server: Option<String>,
    /// SMTP port.
    pub smtp_port: Option<u16>,
    /// Credential source (defaults to [`CredentialSource::None`]).
    pub credential: CredentialSource,
}

/// Create an account.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an empty name; [`Error::AlreadyExists`] for a
/// duplicate name; otherwise a mapped storage error.
pub async fn create(db: &Database, new: NewAccount) -> Result<Account> {
    let name = new.name.trim().to_owned();
    if name.is_empty() {
        return Err(Error::invalid_argument("account name must not be empty"));
    }
    // A Keychain credential is looked up by (service, username), so fail fast
    // rather than persisting an account that can never resolve its password.
    if matches!(new.credential, CredentialSource::Keychain(_))
        && new.username.as_deref().map_or(true, str::is_empty)
    {
        return Err(Error::invalid_argument(
            "keychain credentials require a username",
        ));
    }

    let repo_new = repo::NewAccount {
        name: name.clone(),
        imap_server: new.imap_server,
        imap_port: new.imap_port,
        username: new.username,
        smtp_server: new.smtp_server,
        smtp_port: new.smtp_port,
        secret_kind: Some(new.credential.kind().to_owned()),
        secret_ref: new.credential.reference().map(str::to_owned),
    };

    let id = db
        .write(move |c| repo::insert_account(c, &repo_new))
        .await
        .map_err(|e| map_constraint(e, &name))?;

    get(db, id).await
}

/// List all accounts (ordered by name).
///
/// # Errors
///
/// A mapped storage error.
pub async fn list(db: &Database) -> Result<Vec<Account>> {
    let rows = db.read(repo::list_accounts).await?;
    rows.into_iter().map(Account::from_repo).collect()
}

/// Fetch an account by id.
///
/// # Errors
///
/// [`Error::NotFound`] if no such account; otherwise a mapped storage error.
pub async fn get(db: &Database, id: i64) -> Result<Account> {
    let row = db
        .read(move |c| repo::get_account(c, id))
        .await?
        .ok_or_else(|| Error::not_found(format!("account {id}")))?;
    Account::from_repo(row)
}

/// Delete an account by id (cascades to its mailboxes/messages).
///
/// # Errors
///
/// [`Error::NotFound`] if no such account; otherwise a mapped storage error.
pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let removed = db.write(move |c| repo::delete_account(c, id)).await?;
    if removed {
        Ok(())
    } else {
        Err(Error::not_found(format!("account {id}")))
    }
}

/// Map a UNIQUE-constraint violation on insert to `ALREADY_EXISTS`; otherwise
/// fall back to the default storage→domain mapping.
///
/// `accounts` currently has exactly one insert-time constraint (`UNIQUE(name)`),
/// so any constraint violation here is a duplicate name. Revisit if a future
/// migration adds another CHECK/UNIQUE constraint to the table.
fn map_constraint(err: StorageError, name: &str) -> Error {
    if let StorageError::Sqlite(rusqlite::Error::SqliteFailure(e, _)) = &err {
        if e.code == rusqlite::ErrorCode::ConstraintViolation {
            return Error::already_exists(format!("account named {name:?} already exists"));
        }
    }
    let mapped = Error::from(err);
    debug_assert_ne!(mapped.reason(), ErrorReason::AlreadyExists);
    mapped
}

#[cfg(test)]
mod tests;
