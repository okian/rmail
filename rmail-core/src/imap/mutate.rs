//! IMAP mutation commands: flag replace, move, copy, delete.
//!
//! [`ImapMutator`] is the seam [`crate::mail::MailStore`] mutates the mailbox
//! server through. Production code drives [`LiveImapMutator`], which opens a
//! fresh connection per call via [`conn::connect_account`] — connection
//! reuse/pooling is a possible later optimization, not a correctness
//! requirement, since each mutation is already its own IMAP round trip
//! regardless of whether the socket underneath it is shared. Tests substitute
//! their own [`ImapMutator`]: the real in-process mock this crate uses for its
//! own IMAP tests lives behind `#[cfg(test)] pub(crate) mod mock` (see
//! [`super::mock`]), which is invisible even to this crate's own `tests/`
//! integration binaries, let alone `rmaild`'s — so `rmaild`'s tests inject a
//! lightweight fake instead, and this module's own unit tests (below) are
//! what prove the real wire commands are correct.
//!
//! # `select` + `_via` split
//!
//! Every method is `select` the target mailbox, then one small, stream-generic
//! `..._via` function that does the actual mutation, so the `..._via` half —
//! the part with real IMAP-protocol logic in it — can be driven directly
//! against the in-crate mock in this module's tests, exactly as
//! [`super::conn`]'s own tests drive `login`/`probe_capabilities` against a
//! plain TCP stream rather than a TLS one.
//!
//! # `SELECT` verifies `UIDVALIDITY`, not just that the mailbox opened
//!
//! Every UID this module is ever handed comes from the local mirror, read at
//! some point in the past. If the server has since re-numbered the mailbox
//! (`UIDVALIDITY` bumped — a full-folder rebuild, a migration, some servers do
//! it on any structural change) that UID may now name a *different* message.
//! [`select`] takes the caller's expected `UIDVALIDITY` and fails closed
//! (`FailedPrecondition`) if the server's `SELECT` response disagrees, rather
//! than letting a stale UID silently `STORE`/`EXPUNGE` the wrong mail. This
//! mirrors [`crate::sync::full`]/[`crate::sync::delta`], which both treat a
//! missing `UIDVALIDITY` on `SELECT` as fatal for the same reason.
//!
//! # Every command is bounded by [`super::IMAP_DEADLINE`]
//!
//! Including the best-effort `LOGOUT`: a mutation that already committed
//! (`EXPUNGE` returned) but then hangs waiting for the server to close cleanly
//! must not hold the RPC — and a caller who times out and retries — open
//! indefinitely. [`crate::sync::delta`]/[`crate::sync::full`] bound every IMAP
//! command the same way; [`crate::sync::idle`] specifically bounds `logout`.
//!
//! # `STORE`/`EXPUNGE` go through the raw tagged-completion path, not the
//! # `Fetch`/`Seq` streams
//!
//! [`store`] and [`expunge`] call [`async_imap::Session::run_command_and_check_ok`]
//! directly rather than `Session::uid_store`/`Session::expunge`. Those
//! streaming methods share their untagged-response parser with `uid_fetch`,
//! and this crate already found — see `sync::qresync`'s
//! `a_refused_probe_does_not_look_like_a_quiet_folder` test — that the parser
//! stops at the tagged response *without inspecting its status*: a server
//! answering `NO [LIMIT]` yields zero items and no error, indistinguishable
//! from "nothing to report". That is a fine approximation for a delta-sync
//! probe (the checkpoint just holds and the next pass asks again); it would
//! be silent data loss here, where a caller's whole ordering contract (see
//! [`crate::mail`]'s module docs) depends on knowing whether the IMAP call
//! actually happened before touching anything local.
//!
//! # Move's capability fallback
//!
//! [`move_via`] uses `UID MOVE` when the server advertises the `MOVE`
//! capability (RFC 6851) and otherwise falls back to the
//! `COPY` + `STORE +FLAGS.SILENT (\Deleted)` + `EXPUNGE` sequence RFC 6851
//! itself describes as `MOVE`'s effect. The fallback's `EXPUNGE` removes every
//! `\Deleted` message in the mailbox, not just the one this call flagged —
//! the same caveat [`delete_via`] has, below. It also is not atomic the way a
//! real `MOVE` is: if the `COPY` lands but the `STORE`/`EXPUNGE` that follows
//! it fails, the message now exists in both mailboxes, `\Deleted` but not yet
//! removed, from the source. [`crate::mail::MailStore::move_message`] only
//! drops the local row after this whole sequence returns `Ok`, so that
//! half-failure surfaces as an error and leaves the source's local row
//! exactly where it was — a caller can inspect the mailbox and retry, rather
//! than the local mirror silently disagreeing with a partially-moved message.
//!
//! # Delete does not distinguish its target's `\Deleted` flag from anyone else's
//!
//! `EXPUNGE` removes every message flagged `\Deleted` in the selected
//! mailbox, not just the one this call marked. Precise, single-message
//! removal needs `UID EXPUNGE` (RFC 4315, the `UIDPLUS` capability), which
//! this module does not probe for — [`super::ImapCapabilities`] does not
//! carry a `uidplus` field yet. In practice this is the same right-sized bet
//! the delete-then-expunge idiom in `async-imap`'s own documentation makes: a
//! fresh, single-purpose connection selects the mailbox, flags one message,
//! and expunges immediately, leaving a vanishingly small window for another
//! session to have deleted-but-not-yet-expunged a different message
//! concurrently. Tightening this to `UID EXPUNGE` is a small, self-contained
//! follow-up once capability probing grows a `uidplus` field.

use async_imap::Session;

use super::conn::{self, ImapStream};
use super::{command_error, mailbox_not_found_error, select_error, ImapCapabilities};
use crate::error::Error;
use crate::storage::Database;

/// The IMAP mutation surface [`crate::mail::MailStore`] needs.
///
/// One call = one full round trip (connect, select, mutate, best-effort
/// logout), which keeps every implementor stateless and every call
/// independently retryable — there is no session to lose track of between
/// calls. `uidvalidity` is the caller's expected value (from the local
/// mirror); see the module docs for why every implementation must verify it
/// against the server's `SELECT` response before trusting `uid`.
#[async_trait::async_trait]
pub trait ImapMutator: Send + Sync + std::fmt::Debug {
    /// Replace `uid`'s flags in `mailbox` with exactly `flags` (`STORE
    /// FLAGS`, a full replace — not `+FLAGS`/`-FLAGS`).
    ///
    /// # Errors
    /// A mapped IMAP connection/protocol error; [`Error::FailedPrecondition`]
    /// if the server's `UIDVALIDITY` disagrees with `uidvalidity`.
    async fn set_flags(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        flags: &[String],
    ) -> Result<(), Error>;

    /// Move `uid` from `mailbox` to `dest`.
    ///
    /// # Errors
    /// A mapped IMAP connection/protocol error; [`Error::FailedPrecondition`]
    /// if the server's `UIDVALIDITY` disagrees with `uidvalidity`.
    async fn move_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        dest: &str,
    ) -> Result<(), Error>;

    /// Copy `uid` from `mailbox` to `dest`, leaving the source untouched.
    ///
    /// # Errors
    /// A mapped IMAP connection/protocol error; [`Error::FailedPrecondition`]
    /// if the server's `UIDVALIDITY` disagrees with `uidvalidity`.
    async fn copy_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        dest: &str,
    ) -> Result<(), Error>;

    /// Mark `uid` `\Deleted` and expunge it from `mailbox`.
    ///
    /// # Errors
    /// A mapped IMAP connection/protocol error; [`Error::FailedPrecondition`]
    /// if the server's `UIDVALIDITY` disagrees with `uidvalidity`.
    async fn delete_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
    ) -> Result<(), Error>;
}

/// Drives real IMAP mutations over a fresh connection per call.
#[derive(Debug, Clone)]
pub struct LiveImapMutator {
    db: Database,
}

impl LiveImapMutator {
    /// Create a mutator that resolves account credentials from `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait::async_trait]
impl ImapMutator for LiveImapMutator {
    #[tracing::instrument(skip(self), fields(mailbox = mailbox, uid = uid), err)]
    async fn set_flags(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        flags: &[String],
    ) -> Result<(), Error> {
        let (mut session, _caps) = conn::connect_account(&self.db, account_id).await?;
        let result = set_flags_via(&mut session, mailbox, uidvalidity, uid, flags).await;
        logout(session).await;
        result
    }

    #[tracing::instrument(skip(self), fields(mailbox = mailbox, uid = uid, dest = dest), err)]
    async fn move_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        dest: &str,
    ) -> Result<(), Error> {
        let (mut session, caps) = conn::connect_account(&self.db, account_id).await?;
        let result = move_via(&mut session, caps, mailbox, uidvalidity, uid, dest).await;
        logout(session).await;
        result
    }

    #[tracing::instrument(skip(self), fields(mailbox = mailbox, uid = uid, dest = dest), err)]
    async fn copy_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        dest: &str,
    ) -> Result<(), Error> {
        let (mut session, _caps) = conn::connect_account(&self.db, account_id).await?;
        let result = copy_via(&mut session, mailbox, uidvalidity, uid, dest).await;
        logout(session).await;
        result
    }

    #[tracing::instrument(skip(self), fields(mailbox = mailbox, uid = uid), err)]
    async fn delete_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
    ) -> Result<(), Error> {
        let (mut session, _caps) = conn::connect_account(&self.db, account_id).await?;
        let result = delete_via(&mut session, mailbox, uidvalidity, uid).await;
        logout(session).await;
        result
    }
}

/// Run one IMAP round trip under [`super::IMAP_DEADLINE`], mapping a timeout
/// to [`Error::DeadlineExceeded`] — every command in this module goes through
/// this, including the best-effort logout, so a wedged server after a
/// committed mutation cannot hang the caller. See the module docs.
async fn bounded<T, F>(op: &str, fut: F) -> Result<T, Error>
where
    F: std::future::Future<Output = Result<T, Error>>,
{
    tokio::time::timeout(super::IMAP_DEADLINE, fut)
        .await
        .map_err(|_| Error::deadline_exceeded(format!("{op} timed out")))?
}

/// `SELECT mailbox`, verifying the server's `UIDVALIDITY` matches
/// `expected_uidvalidity` — see the module docs for why a mismatch fails
/// closed rather than proceeding to mutate whatever UID `uid` now names.
async fn select<T: ImapStream>(
    session: &mut Session<T>,
    mailbox: &str,
    expected_uidvalidity: i64,
) -> Result<(), Error> {
    let selected = bounded("SELECT", async {
        session
            .select(mailbox)
            .await
            .map_err(|e| select_error(mailbox, e))
    })
    .await?;

    match selected.uid_validity {
        Some(actual) if i64::from(actual) == expected_uidvalidity => Ok(()),
        Some(actual) => Err(Error::failed_precondition(format!(
            "mailbox {mailbox} UIDVALIDITY changed since this message was last synced \
             (expected {expected_uidvalidity}, server now reports {actual}); a resync will \
             re-key it before this mutation can be retried safely"
        ))),
        None => Err(Error::unavailable(format!(
            "server did not report UIDVALIDITY on SELECT {mailbox}"
        ))),
    }
}

/// `UID STORE <uid> <query>`, checking the tagged completion.
///
/// Deliberately *not* [`async_imap::Session::uid_store`], whose `Stream<Item
/// = Result<Fetch>>` is built on the same untagged-response parser as
/// `uid_fetch` — see the module docs' "`STORE`/`EXPUNGE`..." section.
/// [`Session::run_command_and_check_ok`] is the primitive
/// `copy`/`mv`/`uid_copy`/`uid_mv` already build on internally; using it
/// directly here gets the same tagged-status check for `STORE`/`EXPUNGE`,
/// which have no dedicated non-streaming method.
async fn store<T: ImapStream>(
    session: &mut Session<T>,
    uid: i64,
    query: &str,
) -> Result<(), Error> {
    bounded("UID STORE", async {
        session
            .run_command_and_check_ok(format!("UID STORE {uid} {query}"))
            .await
            .map_err(|e| command_error("UID STORE", e))
    })
    .await
}

/// `EXPUNGE`, checking the tagged completion — see [`store`] for why this is
/// not [`async_imap::Session::expunge`].
async fn expunge<T: ImapStream>(session: &mut Session<T>) -> Result<(), Error> {
    bounded("EXPUNGE", async {
        session
            .run_command_and_check_ok("EXPUNGE")
            .await
            .map_err(|e| command_error("EXPUNGE", e))
    })
    .await
}

/// `UID COPY <uid> <dest>`, quoting `dest`.
///
/// Deliberately not [`async_imap::Session::uid_copy`]: `Session::select` and
/// `Session::uid_mv` both quote their mailbox-name argument through a private
/// `validate_str` helper in `async-imap`, but `uid_copy`/`copy` do not — a gap
/// in that crate, not a deliberate omission (RFC 3501's `mailbox` is a plain
/// `astring`, which a name containing a space is not without quoting). Left
/// alone, a destination like `"Sent Items"` or `"[Gmail]/Sent Mail"` — both
/// entirely ordinary folder names — would reach the wire as two bare, illegal
/// tokens instead of one quoted string.
///
/// A `NO` here (often `[TRYCREATE]`) means `dest` does not exist —
/// [`mailbox_not_found_error`], not the generic retryable [`command_error`].
async fn copy<T: ImapStream>(session: &mut Session<T>, uid: i64, dest: &str) -> Result<(), Error> {
    bounded("UID COPY", async {
        session
            .run_command_and_check_ok(format!("UID COPY {uid} {}", quote_mailbox(dest)))
            .await
            .map_err(|e| mailbox_not_found_error(dest, e))
    })
    .await
}

/// Wrap a mailbox name in an IMAP quoted string, escaping `\` and `"`.
///
/// Not a full `astring`/`literal` implementation — a name containing a
/// control character still is not representable this way — but every mailbox
/// name this crate ever hands in came from a `LIST` response ([`super::folders`])
/// or a caller-supplied plain string, neither of which produces control
/// characters in practice, and quoting is strictly better than the unquoted
/// baseline it replaces.
fn quote_mailbox(name: &str) -> String {
    let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// `SELECT` then `UID STORE FLAGS` (full replace).
async fn set_flags_via<T: ImapStream>(
    session: &mut Session<T>,
    mailbox: &str,
    uidvalidity: i64,
    uid: i64,
    flags: &[String],
) -> Result<(), Error> {
    select(session, mailbox, uidvalidity).await?;
    let query = format!("FLAGS ({})", flags.join(" "));
    store(session, uid, &query).await
}

/// `SELECT` then either `UID MOVE` (when advertised) or the
/// copy/flag/expunge sequence that stands in for it — see the module docs.
async fn move_via<T: ImapStream>(
    session: &mut Session<T>,
    caps: ImapCapabilities,
    mailbox: &str,
    uidvalidity: i64,
    uid: i64,
    dest: &str,
) -> Result<(), Error> {
    select(session, mailbox, uidvalidity).await?;
    if caps.move_ {
        bounded("UID MOVE", async {
            session
                .uid_mv(uid.to_string(), dest)
                .await
                .map_err(|e| mailbox_not_found_error(dest, e))
        })
        .await?;
        return Ok(());
    }

    copy(session, uid, dest).await?;
    store(session, uid, "+FLAGS.SILENT (\\Deleted)").await?;
    expunge(session).await
}

/// `SELECT` then `UID COPY`.
async fn copy_via<T: ImapStream>(
    session: &mut Session<T>,
    mailbox: &str,
    uidvalidity: i64,
    uid: i64,
    dest: &str,
) -> Result<(), Error> {
    select(session, mailbox, uidvalidity).await?;
    copy(session, uid, dest).await
}

/// `SELECT` then `UID STORE +FLAGS.SILENT (\Deleted)` then `EXPUNGE`.
async fn delete_via<T: ImapStream>(
    session: &mut Session<T>,
    mailbox: &str,
    uidvalidity: i64,
    uid: i64,
) -> Result<(), Error> {
    select(session, mailbox, uidvalidity).await?;
    store(session, uid, "+FLAGS.SILENT (\\Deleted)").await?;
    expunge(session).await
}

/// Best-effort logout, bounded like every other command — see the module
/// docs. The connection is dropped regardless of whether this completes.
async fn logout<T: ImapStream>(mut session: Session<T>) {
    let _ = tokio::time::timeout(super::IMAP_DEADLINE, session.logout()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::imap::conn::{login, probe_capabilities};
    use crate::imap::mock::{MockConfig, MockImap};

    /// The mock's default `UIDVALIDITY` (see `MockConfig::default`) — tests
    /// that are not specifically exercising the mismatch path pass this so
    /// `select`'s guard does not reject them.
    const UIDVALIDITY: i64 = 1;

    async fn connect_mock(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
        tokio::net::TcpStream::connect(addr).await.unwrap()
    }

    #[tokio::test]
    async fn set_flags_via_issues_a_full_replace_store() {
        let mock = MockImap::start(MockConfig::default().password("pw").fetch(
            5,
            &["\\Seen"],
            b"body",
        ))
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        set_flags_via(
            &mut session,
            "INBOX",
            UIDVALIDITY,
            5,
            &["\\Seen".to_owned(), "\\Flagged".to_owned()],
        )
        .await
        .expect("set_flags_via should succeed");

        let commands = mock.commands();
        assert!(
            commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case("UID STORE 5 FLAGS (\\Seen \\Flagged)")),
            "expected a full-replace UID STORE, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn select_rejects_a_stale_uidvalidity() {
        // The mock is configured at UIDVALIDITY 1; asking for 999 must fail
        // closed rather than proceed to STORE against a UID that may now name
        // a different message.
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        let err = set_flags_via(&mut session, "INBOX", 999, 5, &["\\Seen".to_owned()])
            .await
            .expect_err("a UIDVALIDITY mismatch must be rejected");
        assert_eq!(err.reason(), crate::ErrorReason::FailedPrecondition);

        // And the STORE must never have been sent — the guard runs before any
        // mutating command, not merely before this one succeeds.
        let commands = mock.commands();
        assert!(
            !commands
                .iter()
                .any(|c| c.to_ascii_uppercase().contains("STORE")),
            "must not attempt to mutate after a UIDVALIDITY mismatch, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn selecting_a_missing_mailbox_is_not_found() {
        let mock = MockImap::start(
            MockConfig::default()
                .password("pw")
                .unselectable("Nonexistent"),
        )
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        let err = set_flags_via(
            &mut session,
            "Nonexistent",
            UIDVALIDITY,
            5,
            &["\\Seen".to_owned()],
        )
        .await
        .expect_err("selecting a missing mailbox must fail");
        assert_eq!(err.reason(), crate::ErrorReason::NotFound);
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn move_via_uses_move_when_the_server_advertises_it() {
        let mock = MockImap::start(
            MockConfig::default().password("pw").fetch(5, &[], b"body"), // MOVE capability is on by default
        )
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();
        let caps = probe_capabilities(&mut session).await.unwrap();
        assert!(caps.move_, "mock advertises MOVE by default");

        move_via(&mut session, caps, "INBOX", UIDVALIDITY, 5, "Archive")
            .await
            .expect("move_via should succeed");

        let commands = mock.commands();
        assert!(
            commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case("UID MOVE 5 \"Archive\"")),
            "expected UID MOVE (async-imap quotes the mailbox name), got: {commands:?}"
        );
        assert!(
            !commands
                .iter()
                .any(|c| c.to_ascii_uppercase().starts_with("UID COPY")),
            "MOVE capability present: must not fall back to COPY, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn move_via_falls_back_to_copy_store_expunge_without_move_capability() {
        let mock = MockImap::start(
            MockConfig::default()
                .password("pw")
                .capabilities(&["IMAP4rev1", "IDLE"])
                .fetch(5, &[], b"body"),
        )
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();
        let caps = probe_capabilities(&mut session).await.unwrap();
        assert!(!caps.move_, "capability list excludes MOVE");

        move_via(&mut session, caps, "INBOX", UIDVALIDITY, 5, "Archive")
            .await
            .expect("move_via should succeed via the fallback");

        let commands = mock.commands();
        assert!(
            commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case("UID COPY 5 \"Archive\"")),
            "expected UID COPY, got: {commands:?}"
        );
        assert!(
            commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case("UID STORE 5 +FLAGS.SILENT (\\Deleted)")),
            "expected a \\Deleted STORE, got: {commands:?}"
        );
        assert!(
            commands.iter().any(|c| c.eq_ignore_ascii_case("EXPUNGE")),
            "expected an EXPUNGE, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn copy_via_issues_uid_copy_and_leaves_the_source_alone() {
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        copy_via(&mut session, "INBOX", UIDVALIDITY, 5, "Archive")
            .await
            .expect("copy_via should succeed");

        let commands = mock.commands();
        assert!(commands
            .iter()
            .any(|c| c.eq_ignore_ascii_case("UID COPY 5 \"Archive\"")));
        assert!(
            !commands
                .iter()
                .any(|c| c.to_ascii_uppercase().contains("DELETED")),
            "copy must never flag the source \\Deleted, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn copy_quotes_a_destination_mailbox_name_containing_a_space() {
        // async-imap's own `uid_copy` does not quote its mailbox argument —
        // see `copy`'s doc comment — so this is this module's own behavior to
        // guarantee, not something inherited for free.
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        copy_via(&mut session, "INBOX", UIDVALIDITY, 5, "Sent Items")
            .await
            .expect("copy_via should succeed");

        let commands = mock.commands();
        assert!(
            commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case("UID COPY 5 \"Sent Items\"")),
            "expected a quoted destination, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn delete_via_marks_deleted_then_expunges() {
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        delete_via(&mut session, "INBOX", UIDVALIDITY, 5)
            .await
            .expect("delete_via should succeed");

        let commands = mock.commands();
        let store_idx = commands
            .iter()
            .position(|c| c.eq_ignore_ascii_case("UID STORE 5 +FLAGS.SILENT (\\Deleted)"))
            .expect("expected a \\Deleted STORE");
        let expunge_idx = commands
            .iter()
            .position(|c| c.eq_ignore_ascii_case("EXPUNGE"))
            .expect("expected an EXPUNGE");
        assert!(
            store_idx < expunge_idx,
            "the STORE must precede the EXPUNGE, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn a_refused_uid_command_maps_to_a_retryable_error() {
        let mock = MockImap::start(
            MockConfig::default()
                .password("pw")
                .fetch(5, &[], b"body")
                .refusing_uid_commands(),
        )
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        let err = set_flags_via(
            &mut session,
            "INBOX",
            UIDVALIDITY,
            5,
            &["\\Seen".to_owned()],
        )
        .await
        .expect_err("a refused UID command must surface as an error");
        assert_eq!(err.reason(), crate::ErrorReason::Unavailable);
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn only_a_trycreate_refusal_means_the_destination_is_missing() {
        // `[TRYCREATE]` is the one refusal RFC 3501 defines as "the
        // destination does not exist", so it is the only one that may map to
        // NOT_FOUND.
        let mock = MockImap::start(
            MockConfig::default()
                .password("pw")
                .fetch(5, &[], b"body")
                .refusing_uid_commands_with_trycreate(),
        )
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();
        let err = copy_via(&mut session, "INBOX", UIDVALIDITY, 5, "Nonexistent")
            .await
            .expect_err("a refused UID COPY must surface as an error");
        assert_eq!(
            err.reason(),
            crate::ErrorReason::NotFound,
            "a [TRYCREATE] refusal names a destination that does not exist"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn a_transient_copy_refusal_stays_retryable() {
        // `[LIMIT]`, `[OVERQUOTA]` and `NO Server busy` all refuse a COPY the
        // server could otherwise serve. Mapping these to NOT_FOUND — as an
        // earlier version of `mailbox_not_found_error` did for every `NO` —
        // tells the client to stop retrying, stranding the move permanently
        // over a full mailbox or a busy server.
        let mock = MockImap::start(
            MockConfig::default()
                .password("pw")
                .fetch(5, &[], b"body")
                .refusing_uid_commands(),
        )
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();
        let err = copy_via(&mut session, "INBOX", UIDVALIDITY, 5, "Archive")
            .await
            .expect_err("a refused UID COPY must surface as an error");
        assert_eq!(
            err.reason(),
            crate::ErrorReason::Unavailable,
            "a transient refusal must stay retryable, not read as a missing \
             destination"
        );
        let _ = session.logout().await;
    }
}
