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
//! # `store_keyword` is a delta `STORE`, not `set_flags`' full replace
//!
//! [`store_keyword`](ImapMutator::store_keyword) is task 55's addition, the
//! model [`set_flags`](ImapMutator::set_flags) it is a sibling of rather
//! than a copy: applying or removing one tag must never disturb any other
//! flag or tag already on the same message the way a full-replace `STORE
//! FLAGS` would, so this issues `+FLAGS.SILENT (kw)` / `-FLAGS.SILENT (kw)`
//! instead — an additive/subtractive delta. It also takes a *set* of UIDs,
//! not one: task 55's bulk-tag path coalesces every message that shares a
//! mailbox into a single `STORE` over a compact UID set (`5,7:9,12`) rather
//! than one round trip per message, and `set_flags`'s one-UID-at-a-time
//! shape has no room for that. A single-message apply is simply the `uids.len()
//! == 1` case of the same call, not a separate code path.
//!
//! Gmail's non-standard `X-GM-LABELS` item is used instead of `FLAGS` when
//! `prefer_gmail_label` is set *and* the live session actually advertises
//! `X-GM-EXT-1` — checked fresh per call (via [`Session::capabilities`]) not
//! carried on the [`ImapCapabilities`] this connection also returns from
//! [`conn::connect_account`] at login: adding a `gmail` field to that struct
//! would touch every one of its many existing call sites across
//! `crate::sync` (most of them hand-built test fixtures with no
//! `..Default::default()`) for a fact only this one method needs — a self-
//! contained, redundant capability check here costs one extra `CAPABILITY`
//! round trip and touches nothing else.
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
use super::{command_error, mailbox_not_found_error, map_imap_err, select_error, ImapCapabilities};
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

    /// Add (`add = true`) or remove (`add = false`) one keyword/label on
    /// every UID in `uids` with a single coalesced `STORE` — a delta, not a
    /// full replace. See the module docs' "`store_keyword` is a delta
    /// `STORE`..." section for why this differs from
    /// [`set_flags`](Self::set_flags) in both regards, and for
    /// `prefer_gmail_label`'s exact meaning.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if `uids` is empty or `keyword` contains a
    /// control character (unsafe to interpolate into a command line —
    /// see [`validate_keyword`]); [`Error::FailedPrecondition`] if the
    /// server's `UIDVALIDITY` disagrees with `uidvalidity`; otherwise a
    /// mapped IMAP connection/protocol error (a refused `STORE` — including
    /// one naming an unsupported item like `X-GM-LABELS` on a non-Gmail
    /// server — surfaces here as an ordinary retryable error, exactly what
    /// [`crate::tags::sync`]'s `auto` downgrade watches for).
    #[allow(clippy::too_many_arguments)]
    async fn store_keyword(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uids: &[i64],
        keyword: &str,
        prefer_gmail_label: bool,
        add: bool,
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

    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(
        skip(self, uids, keyword),
        fields(mailbox = mailbox, uids = uids.len(), add = add),
        err
    )]
    async fn store_keyword(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uids: &[i64],
        keyword: &str,
        prefer_gmail_label: bool,
        add: bool,
    ) -> Result<(), Error> {
        let (mut session, _caps) = conn::connect_account(&self.db, account_id).await?;
        // Checked fresh, per call, rather than threaded from the
        // `ImapCapabilities` `connect_account` also returned — see the
        // module docs' "`store_keyword` is a delta `STORE`..." section.
        let gmail = if prefer_gmail_label {
            match bounded("CAPABILITY", async {
                session.capabilities().await.map_err(map_imap_err)
            })
            .await
            {
                Ok(caps) => caps.has_str("X-GM-EXT-1"),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "capability probe for X-GM-EXT-1 failed; treating as non-Gmail"
                    );
                    false
                }
            }
        } else {
            false
        };
        let result = store_keyword_via(
            &mut session,
            gmail,
            mailbox,
            uidvalidity,
            uids,
            keyword,
            add,
        )
        .await;
        logout(session).await;
        result
    }
}

/// Run one IMAP round trip under [`super::IMAP_DEADLINE`], mapping a timeout
/// to [`Error::DeadlineExceeded`] — every command in this module goes through
/// this, including the best-effort logout, so a wedged server after a
/// committed mutation cannot hang the caller. See the module docs.
///
/// A timeout drops the command future mid-protocol, which leaves the session
/// with an unread reply still in flight and therefore in an undefined
/// read/write state. Callers must treat a timed-out session as spent and
/// discard it rather than issuing another command on it; the only thing this
/// module does with one afterwards is the best-effort logout, itself bounded,
/// so the worst case is one further `IMAP_DEADLINE` before the socket closes.
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
    store_set(session, &uid.to_string(), query).await
}

/// `UID STORE <uid-set> <query>`, checking the tagged completion — the
/// multi-UID generalization of [`store`] (which is now a thin wrapper over
/// this for the single-UID case), added for
/// [`store_keyword_via`]'s coalesced apply.
async fn store_set<T: ImapStream>(
    session: &mut Session<T>,
    uid_set: &str,
    query: &str,
) -> Result<(), Error> {
    bounded("UID STORE", async {
        session
            .run_command_and_check_ok(format!("UID STORE {uid_set} {query}"))
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

/// `SELECT` then a coalesced `UID STORE {+|-}<item> (<keyword>)` over every
/// UID in `uids` — the delta apply/remove [`store_keyword`](super::
/// ImapMutator::store_keyword) drives. `gmail` selects `X-GM-LABELS` over
/// plain `FLAGS.SILENT`; see that method's docs for why this takes an
/// already-resolved `bool` rather than probing capabilities itself (the
/// caller does that, once, before opening this — or, in a test, simply
/// knows what the mock advertises).
///
/// `pub(crate)` rather than private: this is what
/// `crate::tags`' own tests drive directly against
/// [`super::mock`](crate::imap::mock)'s real, plaintext TCP server (manually
/// logged in, bypassing [`LiveImapMutator`]'s TLS-only [`conn::connect_account`]
/// entirely) to prove the auto-downgrade path is driven by a genuine IMAP
/// `NO`, not a test double — see `crate::tags::sync`'s own module docs. This
/// mirrors [`set_flags_via`]'s own tests below, which drive it the identical
/// way for the same reason.
///
/// # Errors
/// [`Error::InvalidArgument`] if `uids` is empty or `keyword` contains a
/// control character; otherwise as [`select`]/[`store_set`].
pub(crate) async fn store_keyword_via<T: ImapStream>(
    session: &mut Session<T>,
    gmail: bool,
    mailbox: &str,
    uidvalidity: i64,
    uids: &[i64],
    keyword: &str,
    add: bool,
) -> Result<(), Error> {
    if uids.is_empty() {
        return Err(Error::invalid_argument(
            "store_keyword requires at least one uid",
        ));
    }
    validate_keyword(keyword)?;
    // RFC 3501's `flag-keyword` production is `atom` -- full stop, never a
    // quoted string -- so unlike `X-GM-LABELS` (Gmail's own extension,
    // whose label list accepts `atom / string`), there is no legal way to
    // send a non-atom-safe keyword through a plain `FLAGS`/`FLAGS.SILENT`
    // STORE. An earlier version of this function quoted it anyway, which
    // produced a syntactically invalid command a compliant server answers
    // `BAD` to -- silently, since nothing here inspected the response
    // differently. Rejecting up front turns that into an immediate,
    // diagnosable `Err` instead: exactly the shape `sync::apply_wire`'s
    // `auto` downgrade already knows how to react to, and a clear message
    // under `sync_mode = imap` instead of a mysterious server `BAD`.
    if !gmail && !keyword.chars().all(is_atom_char) {
        return Err(Error::invalid_argument(format!(
            "{keyword:?} is not a valid IMAP keyword atom (RFC 3501 flag-keyword = atom, \
             never a quoted string) — rename the tag, or use sync_mode=auto/local, or a \
             Gmail account with X-GM-LABELS"
        )));
    }
    select(session, mailbox, uidvalidity).await?;
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let uid_set = render_uid_set(&sorted);
    let verb = if add { '+' } else { '-' };
    let item = if gmail { "X-GM-LABELS" } else { "FLAGS.SILENT" };
    let query = format!("{verb}{item} ({})", quote_keyword(keyword));
    store_set(session, &uid_set, &query).await
}

/// Whether `c` is a valid IMAP `ATOM-CHAR` (RFC 3501): any non-control
/// ASCII character except the `atom-specials` — `(`, `)`, `{`, space,
/// `%`/`*` (list wildcards), `"`/`\` (quoted-specials), and `]`
/// (resp-specials). Deliberately closer to the real grammar than a narrow
/// `[A-Za-z0-9-_./]` allow-list: a tag keyword legitimately contains `/`
/// (hierarchy) and this admits the rest of what RFC 3501 actually permits
/// too (`$`, `+`, `#`, ...) rather than forcing every such name through
/// [`quote_keyword`]'s quoted-string fallback, which [`store_keyword_via`]
/// now rejects outright for the plain-`FLAGS` case (a quoted string is
/// simply not a legal `flag-keyword`, ever — see that function's own docs).
fn is_atom_char(c: char) -> bool {
    c.is_ascii_graphic() && !matches!(c, '(' | ')' | '{' | '%' | '*' | '"' | '\\' | ']')
}

/// Reject a keyword/label containing an ASCII control character — in
/// particular CR/LF, which (unlike `\`/`"`) an IMAP quoted string cannot
/// escape at all: a literal newline inside one would break the line-based
/// protocol and could smuggle a second command, the same command-injection
/// concern [`crate::mail::is_safe_flag`] guards a full-replace `FLAGS`
/// argument against. Everything else is admitted (and, if not atom-safe,
/// quoted by [`quote_keyword`]) — a hierarchical tag keyword legitimately
/// contains `/` (`rmail/project/alpha`), and a Gmail label may contain
/// spaces, which `is_safe_flag`'s narrower allow-list does not.
fn validate_keyword(value: &str) -> Result<(), Error> {
    if value.is_empty() || value.chars().any(|c| c.is_control()) {
        return Err(Error::invalid_argument(format!(
            "{value:?} is not a valid IMAP keyword/label"
        )));
    }
    Ok(())
}

/// Render `value` as a bare IMAP atom when every character is atom-safe
/// ([`is_atom_char`]), or as a quoted string (escaping `\`/`"`, mirroring
/// [`quote_mailbox`]) otherwise. The quoted-string fallback is only ever
/// reached for a Gmail label (`X-GM-LABELS` accepts `atom / string`) — the
/// plain-`FLAGS` caller in [`store_keyword_via`] rejects a non-atom-safe
/// keyword before this is even called, since a quoted string is not a
/// legal `flag-keyword` at all. Callers must run [`validate_keyword`]
/// first; this does not itself reject a control character (quoting cannot
/// make one safe — see that function's docs).
fn quote_keyword(value: &str) -> String {
    if value.chars().all(is_atom_char) {
        value.to_owned()
    } else {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    }
}

/// Render a sorted, deduped list of UIDs as a compact IMAP UID set
/// (`5`, `1:3,7`, ...) — the client-request-side counterpart to
/// [`super::mock`](crate::imap::mock)'s response-side renderer of the same
/// shape, used here to build a coalesced multi-message `STORE`'s argument.
fn render_uid_set(uids: &[i64]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut iter = uids.iter().copied();
    let Some(mut start) = iter.next() else {
        return String::new();
    };
    let mut end = start;
    for uid in iter {
        if uid == end + 1 {
            end = uid;
        } else {
            parts.push(render_range(start, end));
            start = uid;
            end = uid;
        }
    }
    parts.push(render_range(start, end));
    parts.join(",")
}

fn render_range(start: i64, end: i64) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
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

    // -----------------------------------------------------------------------
    // store_keyword_via (task 55)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn store_keyword_via_issues_a_delta_store_not_a_full_replace() {
        let mock = MockImap::start(MockConfig::default().password("pw").fetch(
            5,
            &["\\Seen"],
            b"body",
        ))
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        store_keyword_via(
            &mut session,
            false,
            "INBOX",
            UIDVALIDITY,
            &[5],
            "rmail/work",
            true,
        )
        .await
        .expect("store_keyword_via should succeed");

        let commands = mock.commands();
        assert!(
            commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case("UID STORE 5 +FLAGS.SILENT (rmail/work)")),
            "expected an additive keyword STORE, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn store_keyword_via_removes_with_a_minus_flags_delta() {
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        store_keyword_via(
            &mut session,
            false,
            "INBOX",
            UIDVALIDITY,
            &[5],
            "rmail/work",
            false,
        )
        .await
        .expect("store_keyword_via should succeed");

        let commands = mock.commands();
        assert!(
            commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case("UID STORE 5 -FLAGS.SILENT (rmail/work)")),
            "expected a subtractive keyword STORE, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn store_keyword_via_coalesces_multiple_uids_into_one_store() {
        // Task 55's bulk-tag acceptance: N messages sharing a mailbox get one
        // STORE over a compact UID set, not N round trips.
        let mock = MockImap::start(
            MockConfig::default()
                .password("pw")
                .fetch(1, &[], b"a")
                .fetch(2, &[], b"b")
                .fetch(3, &[], b"c")
                .fetch(7, &[], b"d"),
        )
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        store_keyword_via(
            &mut session,
            false,
            "INBOX",
            UIDVALIDITY,
            &[3, 1, 2, 7],
            "rmail/urgent",
            true,
        )
        .await
        .expect("store_keyword_via should succeed");

        let commands = mock.commands();
        let store_commands: Vec<&String> = commands
            .iter()
            .filter(|c| c.to_ascii_uppercase().starts_with("UID STORE"))
            .collect();
        assert_eq!(
            store_commands.len(),
            1,
            "expected exactly one coalesced STORE, got: {commands:?}"
        );
        assert!(
            store_commands[0].eq_ignore_ascii_case("UID STORE 1:3,7 +FLAGS.SILENT (rmail/urgent)"),
            "expected a compact UID set, got: {}",
            store_commands[0]
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn store_keyword_via_prefers_gmail_labels_when_asked() {
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        store_keyword_via(&mut session, true, "INBOX", UIDVALIDITY, &[5], "work", true)
            .await
            .expect("store_keyword_via should succeed");

        let commands = mock.commands();
        assert!(
            commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case("UID STORE 5 +X-GM-LABELS (work)")),
            "expected an X-GM-LABELS STORE, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn store_keyword_via_quotes_a_label_containing_a_space() {
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        store_keyword_via(
            &mut session,
            true,
            "INBOX",
            UIDVALIDITY,
            &[5],
            "Q3 Report",
            true,
        )
        .await
        .expect("store_keyword_via should succeed");

        let commands = mock.commands();
        assert!(
            commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case("UID STORE 5 +X-GM-LABELS (\"Q3 Report\")")),
            "expected a quoted label, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn store_keyword_via_rejects_a_non_atom_keyword_against_plain_flags() {
        // RFC 3501's `flag-keyword` is always `atom`, never a quoted
        // string -- unlike the Gmail-only test above (`gmail = true`),
        // sending "Q3 Report" through plain `FLAGS.SILENT` (`gmail =
        // false`) has no legal wire form at all, so this must be rejected
        // up front rather than silently quoted into a malformed command.
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        let err = store_keyword_via(
            &mut session,
            false,
            "INBOX",
            UIDVALIDITY,
            &[5],
            "Q3 Report",
            true,
        )
        .await
        .expect_err("a non-atom-safe keyword must be rejected for plain FLAGS");
        assert_eq!(err.reason(), crate::ErrorReason::InvalidArgument);
        assert!(
            mock.commands()
                .iter()
                .all(|c| !c.to_ascii_uppercase().contains("SELECT")),
            "the rejection must happen before SELECT, not after a malformed STORE"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn store_keyword_via_rejects_an_empty_uid_list() {
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        let err = store_keyword_via(&mut session, false, "INBOX", UIDVALIDITY, &[], "work", true)
            .await
            .expect_err("an empty uid list must be rejected");
        assert_eq!(err.reason(), crate::ErrorReason::InvalidArgument);
        // `LOGIN` already happened to establish `session` above; what this
        // asserts is that the rejection short-circuits *before* `select`, not
        // that the connection sent nothing at all.
        let commands = mock.commands();
        assert!(
            !commands
                .iter()
                .any(|c| c.to_ascii_uppercase().contains("SELECT")
                    || c.to_ascii_uppercase().contains("STORE")),
            "no SELECT/STORE should be sent for a rejected call, got: {commands:?}"
        );
        let _ = session.logout().await;
    }

    #[tokio::test]
    async fn store_keyword_via_rejects_a_keyword_containing_a_control_character() {
        let mock =
            MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        let err = store_keyword_via(
            &mut session,
            false,
            "INBOX",
            UIDVALIDITY,
            &[5],
            "work\r\nA1 LOGOUT",
            true,
        )
        .await
        .expect_err("a control character must be rejected, not smuggled onto the wire");
        assert_eq!(err.reason(), crate::ErrorReason::InvalidArgument);
        let _ = session.logout().await;
    }

    /// The proof the acceptance criterion asks for by name: this is a real
    /// IMAP `NO` from the mock server, not a boolean a test set itself. This
    /// alone proves the wire-level contract `store_keyword` (and therefore
    /// `crate::tags::sync`'s `auto`-downgrade decision, which reacts to
    /// exactly this shape of `Err`) depends on.
    #[tokio::test]
    async fn a_refused_keyword_store_maps_to_a_retryable_error() {
        let mock = MockImap::start(
            MockConfig::default()
                .password("pw")
                .fetch(5, &[], b"body")
                .refusing_uid_commands(),
        )
        .await;
        let stream = connect_mock(mock.addr).await;
        let mut session = login(stream, "user", "pw").await.unwrap();

        let err = store_keyword_via(
            &mut session,
            false,
            "INBOX",
            UIDVALIDITY,
            &[5],
            "work",
            true,
        )
        .await
        .expect_err("a server NO must surface as an error");
        assert_eq!(err.reason(), crate::ErrorReason::Unavailable);
        let _ = session.logout().await;
    }

    #[test]
    fn render_uid_set_compacts_consecutive_runs() {
        assert_eq!(render_uid_set(&[1, 2, 3, 7]), "1:3,7");
        assert_eq!(render_uid_set(&[5]), "5");
        assert_eq!(render_uid_set(&[1, 3, 5]), "1,3,5");
        assert_eq!(render_uid_set(&[]), "");
    }
}
