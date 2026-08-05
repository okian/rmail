//! The IMAP IDLE push engine: a long-lived connection per watched folder.
//!
//! Polling is a latency/cost trade nobody wins. A five-minute poll means mail
//! is up to five minutes late; a five-second poll means a thousand pointless
//! round trips an hour per folder. `IDLE` (RFC 2177) removes the trade: the
//! client parks a connection and the server speaks when something happens.
//!
//! # The shape of a watch
//!
//! ```text
//!   connect ─▶ enable QRESYNC ─▶ [ delta sync ─▶ IDLE ─▶ woken ] ─▶ …
//!       ▲                                                  │
//!       └──────────── backoff ◀──── connection lost ◀───────┘
//! ```
//!
//! Every wake-up runs a [`crate::sync::delta`] pass. That is deliberate: `IDLE`
//! reports only *that* something changed, never what, so the engine that knows
//! how to ask cheaply is the one that answers. New mail, a flag flipped on
//! another device, and an expunge all arrive the same way and are all resolved
//! by the same modseq probe.
//!
//! # Why a watch never simply stops
//!
//! A long-lived connection is a connection that will be dropped — by a NAT
//! timeout, a server restart, a laptop lid. So the loop treats disconnection as
//! routine, not exceptional: it reconnects with exponential backoff and keeps
//! going. The alternative, a watcher that exits on the first broken pipe, is a
//! mail client that silently stops receiving mail and looks perfectly healthy
//! while doing so.
//!
//! Two things bound the parking:
//!
//! - **Re-IDLE.** RFC 2177 §3 warns that a server may log off a client whose
//!   `IDLE` has run too long, so the command is torn down and reissued on a
//!   cadence ([`IdleOptions::re_idle`], clamped to [`MAX_IDLE`]). A re-IDLE is
//!   also a liveness check: a connection that died quietly fails here rather
//!   than parking forever on a socket nobody is listening to.
//! - **Poll fallback.** A server without `IDLE` — or a run with it switched off
//!   — gets [`IdleOptions::poll_interval`] instead. Same loop, same delta pass,
//!   worse latency. Nothing above this module needs to know which one it got.
//!
//! # Cancellation
//!
//! `IDLE` is the one place in the sync engine where blocking forever is the
//! *intended* behaviour, so it is also the one place that must be interruptible
//! without waiting for a timeout. Dropping async-imap's `StopSource` ends the
//! wait, and the loop then sends `DONE` and leaves the session clean — a
//! shutdown that abandoned the command mid-flight would leave the server
//! holding an `IDLE` it never saw terminated.

use std::time::Duration;

use async_imap::extensions::idle::IdleResponse;
use async_imap::Session;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::imap::conn::ImapStream;
use crate::imap::ImapCapabilities;
use crate::repo;
use crate::storage::Database;

use super::delta::{self, DeltaReport};
use super::full::SyncOptions;
use super::ChangeSink;

/// RFC 2177 §3: re-issue `IDLE` at least every 29 minutes so a server with an
/// inactivity timeout does not log the client off mid-park.
pub const MAX_IDLE: Duration = Duration::from_secs(29 * 60);

/// Default cadence for tearing down and reissuing `IDLE`.
pub const DEFAULT_RE_IDLE: Duration = Duration::from_secs(5 * 60);

/// Default interval between passes when `IDLE` is unavailable.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// First reconnect delay after a connection drops.
pub const DEFAULT_BACKOFF_MIN: Duration = Duration::from_secs(1);

/// Ceiling on the reconnect delay. Long enough not to hammer a server that is
/// down, short enough that a laptop waking from sleep reconnects promptly.
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// How many of an account's folders are watched by default.
///
/// "High-priority" is what [`crate::sync::full::prioritize`] already means:
/// INBOX, then the well-known folders. Watching every folder of a large account
/// would exhaust the server's per-account connection limit and get the rest of
/// them refused.
pub const DEFAULT_WATCH_LIMIT: usize = 5;

/// Tuning for a folder watch.
#[derive(Debug, Clone, Copy)]
pub struct IdleOptions {
    /// How long one `IDLE` may run before it is torn down and reissued.
    /// Clamped to [`MAX_IDLE`].
    pub re_idle: Duration,
    /// Interval between passes when `IDLE` is unavailable.
    pub poll_interval: Duration,
    /// First delay after a dropped connection; doubles up to [`Self::backoff_max`].
    pub backoff_min: Duration,
    /// Ceiling on the reconnect delay.
    pub backoff_max: Duration,
    /// Tuning handed to each delta pass.
    pub sync: SyncOptions,
    /// How many of an account's folders [`watch_folders`] keeps connections
    /// for. Each watch is a socket the server holds open, and servers cap
    /// concurrent connections per account — Gmail at 15, most others lower — so
    /// this is a budget, not a preference.
    pub watch_limit: usize,
}

impl Default for IdleOptions {
    fn default() -> Self {
        Self {
            re_idle: DEFAULT_RE_IDLE,
            poll_interval: DEFAULT_POLL_INTERVAL,
            backoff_min: DEFAULT_BACKOFF_MIN,
            backoff_max: DEFAULT_BACKOFF_MAX,
            sync: SyncOptions::default(),
            watch_limit: DEFAULT_WATCH_LIMIT,
        }
    }
}

impl IdleOptions {
    /// The `IDLE` duration actually used, clamped to what RFC 2177 allows.
    #[must_use]
    pub fn effective_re_idle(&self) -> Duration {
        self.re_idle.min(MAX_IDLE).max(Duration::from_millis(1))
    }
}

/// Why a watch cycle ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchTrigger {
    /// The first pass after connecting — the folder is caught up before the
    /// watch parks, so a client never waits on `IDLE` for mail that had already
    /// arrived.
    Initial,
    /// The server pushed something during `IDLE`.
    Pushed,
    /// The `IDLE` was reissued on cadence and the pass ran with it.
    ReIdle,
    /// `IDLE` is unavailable; this was a poll tick.
    Polled,
    /// The connection dropped and was re-established.
    Reconnected,
}

impl WatchTrigger {
    /// The trigger name, for logs and reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::Pushed => "pushed",
            Self::ReIdle => "re-idle",
            Self::Polled => "polled",
            Self::Reconnected => "reconnected",
        }
    }
}

/// One observation from a watch, emitted per cycle.
#[derive(Debug, Clone)]
pub struct WatchCycle {
    /// The mailbox being watched.
    pub mailbox_id: i64,
    /// Why this cycle ran.
    pub trigger: WatchTrigger,
    /// What the delta pass found, or `None` if the pass itself failed.
    pub report: Option<DeltaReport>,
    /// Whether this folder is being watched by `IDLE` (`false` = polling).
    pub pushing: bool,
}

/// Why a watch returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchOutcome {
    /// The cancellation token fired; the session was left clean.
    Cancelled,
    /// Reconnection was abandoned — see [`WatchReport::connect_failures`].
    GaveUp,
}

/// The outcome of a folder watch.
#[derive(Debug, Clone)]
pub struct WatchReport {
    /// The mailbox watched.
    pub mailbox_id: i64,
    /// Why the watch returned.
    pub outcome: WatchOutcome,
    /// Cycles that ran.
    pub cycles: u64,
    /// Delta passes that returned an error. A failing pass does not end the
    /// watch — the next push or tick tries again.
    pub sync_failures: u64,
    /// Consecutive failures that cannot improve with time (a deleted folder,
    /// revoked credentials) at the moment the watch returned. Transient ones do
    /// not count — those retry indefinitely.
    pub permanent_failures: u32,
    /// Whether the watch ever actually parked on `IDLE` and came back — not
    /// merely whether the server advertised it.
    pub used_idle: bool,
}
/// How many consecutive *permanent* failures a watch tolerates before giving
/// up.
///
/// Only permanent ones count. A folder the server has deleted, or an account
/// whose password was revoked, will never succeed however long the watch waits,
/// so retrying it forever is a busy loop with a mail-client-shaped wrapper. A
/// server that is merely down is a different thing entirely: that retries
/// indefinitely at [`IdleOptions::backoff_max`], because a watch that gave up
/// during an outage is a mailbox that silently stops receiving mail.
pub const MAX_PERMANENT_FAILURES: u32 = 3;

/// How many consecutive `IDLE` failures before a watch stops trying to push and
/// falls back to polling for the rest of its life.
///
/// A server may advertise `IDLE` and then refuse it — an upgrade, a proxy in
/// the path, a per-connection limit. Reconnecting forever to re-attempt a
/// command that keeps failing is worse than the polling it was avoiding.
pub const MAX_IDLE_FAILURES: u32 = 3;

/// Whether an error will still be an error however long the watch waits.
fn is_permanent(error: &Error) -> bool {
    use crate::ErrorReason as R;
    matches!(
        error.reason(),
        R::NotFound
            | R::Unauthenticated
            | R::PermissionDenied
            | R::InvalidArgument
            | R::FailedPrecondition
    )
}

/// Watch one folder, keeping it in sync as the server changes it.
///
/// Runs until `cancel` fires or the folder proves permanently unwatchable.
/// `connect` is called for the first session and again after every drop, so the
/// caller owns how a session is made (TLS, credentials, and their refresh) and
/// the watch owns when. Each cycle is reported through `on_cycle`.
///
/// The loop is written so that no *transient* failure ends it: a delta pass
/// that errors, a refused `IDLE`, a dropped connection — each backs off and
/// tries again, and the backoff only resets after a cycle that actually
/// succeeded, so a persistent post-connect failure decays to one attempt per
/// [`IdleOptions::backoff_max`] rather than hammering the server. Only
/// cancellation, or [`MAX_PERMANENT_FAILURES`] consecutive failures that cannot
/// improve with time, return.
///
/// # Errors
///
/// [`Error::NotFound`] if `mailbox_id` does not exist, or a mapped storage
/// error reading it. Both are checked before the first connection, so a watch
/// that returns `Err` never started; everything after that is handled inside
/// the loop and surfaced through [`WatchReport`].
// Every one of these is load-bearing and none groups naturally with
// another; a struct here would trade an honest signature for indirection.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    skip(db, connect, capabilities, opts, cancel, on_cycle, sink),
    fields(folder, account_id)
)]
pub async fn watch_folder<T, C, Fut, F>(
    db: &Database,
    mailbox_id: i64,
    capabilities: ImapCapabilities,
    opts: IdleOptions,
    cancel: &CancellationToken,
    mut connect: C,
    mut on_cycle: F,
    sink: &mut impl ChangeSink,
) -> Result<WatchReport, Error>
where
    T: ImapStream,
    C: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Session<T>, Error>>,
    F: FnMut(WatchCycle),
{
    // Fail fast on a mailbox that does not exist. Discovering that by watching
    // it would mean an endless loop of connect-select-fail against a folder
    // that is never coming back.
    let mailbox = db
        .read(move |c| repo::get_mailbox(c, mailbox_id))
        .await?
        .ok_or_else(|| Error::not_found(format!("mailbox {mailbox_id} not found")))?;
    let span = tracing::Span::current();
    span.record("folder", tracing::field::display(&mailbox.name));
    span.record("account_id", mailbox.account_id);

    let mut report = WatchReport {
        mailbox_id,
        outcome: WatchOutcome::Cancelled,
        cycles: 0,
        sync_failures: 0,
        permanent_failures: 0,
        used_idle: false,
    };
    // One counter, one backoff, covering every way a cycle can fail. Keeping
    // them per-stage is how a watch ends up hammering a server: connecting
    // resets what the failure after it just raised.
    let mut backoff = opts.backoff_min;
    let mut pushing = capabilities.idle;
    let mut idle_failures = 0u32;
    let mut first_connection = true;

    macro_rules! setback {
        ($error:expr) => {{
            let error: &Error = $error;
            if is_permanent(error) {
                report.permanent_failures += 1;
                if report.permanent_failures >= MAX_PERMANENT_FAILURES {
                    tracing::error!(
                        %error,
                        attempts = report.permanent_failures,
                        "giving up on this folder watch: the failure cannot improve with time"
                    );
                    report.outcome = WatchOutcome::GaveUp;
                    return Ok(report);
                }
            } else {
                report.permanent_failures = 0;
            }
            if sleep_or_cancel(backoff, cancel).await.is_cancelled() {
                return Ok(report);
            }
            backoff = (backoff * 2).min(opts.backoff_max);
        }};
    }

    'reconnect: loop {
        if cancel.is_cancelled() {
            return Ok(report);
        }

        // Racing the connect against cancellation matters: a blackholed SYN
        // pins the OS connect timeout, which is over a minute on every platform
        // that matters, and a shutdown should not wait it out.
        let connected = tokio::select! {
            result = connect() => result,
            () = cancel.cancelled() => return Ok(report),
        };
        let mut session = match connected {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!(%error, ?backoff, "watch connection failed; backing off");
                setback!(&error);
                continue 'reconnect;
            }
        };

        // QRESYNC is a session-level switch (RFC 5161 §3.1) and this is the
        // only moment the session has no mailbox selected.
        let capabilities =
            delta::enable_qresync(&mut session, capabilities, opts.sync.window_timeout).await;

        let mut trigger = if first_connection {
            WatchTrigger::Initial
        } else {
            WatchTrigger::Reconnected
        };
        first_connection = false;

        loop {
            // Sync first, park second. Parking on IDLE before catching up would
            // leave mail that arrived while disconnected sitting undelivered
            // until the *next* thing happened — which on a quiet folder could
            // be hours.
            let outcome = delta::delta_sync(
                &mut session,
                db,
                mailbox_id,
                capabilities,
                opts.sync,
                cancel,
                sink,
            )
            .await;
            match &outcome {
                Ok(synced) => tracing::debug!(
                    trigger = trigger.as_str(),
                    new_messages = synced.new_messages,
                    flag_updates = synced.flag_updates,
                    expunged = synced.expunged,
                    "watch cycle synced"
                ),
                Err(error) => {
                    tracing::warn!(%error, trigger = trigger.as_str(), "watch cycle sync failed");
                }
            }
            let failure = outcome.as_ref().err().map(Error::to_string);
            report.cycles += 1;
            if outcome.is_err() {
                report.sync_failures += 1;
            }
            let synced = match outcome {
                Ok(synced) => Some(synced),
                Err(error) => {
                    on_cycle(WatchCycle {
                        mailbox_id,
                        trigger,
                        report: None,
                        pushing,
                    });
                    if cancel.is_cancelled() {
                        return Ok(report);
                    }
                    // A mapped IMAP error usually leaves the session
                    // mid-command; reconnecting is cheaper than reasoning about
                    // which errors left it clean.
                    setback!(&error);
                    continue 'reconnect;
                }
            };
            let _ = failure;
            on_cycle(WatchCycle {
                mailbox_id,
                trigger,
                report: synced,
                pushing,
            });

            if cancel.is_cancelled() {
                let _ = logout(session, opts.sync.window_timeout).await;
                return Ok(report);
            }

            if pushing {
                tracing::info!(
                    re_idle = ?opts.effective_re_idle(),
                    "watch parked on IDLE"
                );
                match park_on_idle(session, opts, cancel).await {
                    Parked::Woken {
                        session: resumed,
                        pushed,
                    } => {
                        session = resumed;
                        report.used_idle = true;
                        idle_failures = 0;
                        // A cycle that got all the way to a park is a working
                        // cycle; that is the only thing that earns a reset.
                        report.permanent_failures = 0;
                        backoff = opts.backoff_min;
                        trigger = if pushed {
                            WatchTrigger::Pushed
                        } else {
                            WatchTrigger::ReIdle
                        };
                        tracing::info!(trigger = trigger.as_str(), "watch woke");
                    }
                    Parked::Cancelled => return Ok(report),
                    Parked::Lost(error) => {
                        idle_failures += 1;
                        if idle_failures >= MAX_IDLE_FAILURES {
                            // The server advertised IDLE and will not honour
                            // it. Polling is worse than pushing and far better
                            // than a reconnect loop that never settles.
                            tracing::warn!(
                                %error,
                                attempts = idle_failures,
                                "IDLE keeps failing; falling back to polling for this watch"
                            );
                            pushing = false;
                        } else {
                            tracing::warn!(%error, "IDLE connection lost; reconnecting");
                        }
                        setback!(&error);
                        continue 'reconnect;
                    }
                }
            } else {
                report.permanent_failures = 0;
                backoff = opts.backoff_min;
                if sleep_or_cancel(opts.poll_interval, cancel)
                    .await
                    .is_cancelled()
                {
                    let _ = logout(session, opts.sync.window_timeout).await;
                    return Ok(report);
                }
                trigger = WatchTrigger::Polled;
            }
        }
    }
}

/// One folder's failure inside an account-wide watch.
#[derive(Debug)]
pub struct WatchFailure {
    /// The mailbox that could not be watched.
    pub mailbox_id: i64,
    /// Its name, for logs and messages.
    pub name: String,
    /// Why it failed.
    pub error: Error,
}

/// The outcome of watching an account's folders.
#[derive(Debug, Default)]
pub struct AccountWatchReport {
    /// Watches that ran, in priority order.
    pub reports: Vec<WatchReport>,
    /// Folders that could not be watched at all.
    pub failures: Vec<WatchFailure>,
}

/// Watch an account's highest-priority folders concurrently, one connection
/// each.
///
/// [`crate::sync::full::prioritize`] decides what "high priority" means — INBOX
/// first, then the well-known folders — and [`IdleOptions::watch_limit`] caps
/// how many get a connection. That cap is the point: every watch is a socket
/// the server holds open, and servers cap concurrent connections per account,
/// so watching everything would get the folders that matter refused.
///
/// Returns when every watch has returned, which for healthy folders means when
/// `cancel` fires. A folder that cannot be watched at all is collected into
/// [`AccountWatchReport::failures`] and does not stop the others.
///
/// # Errors
///
/// Only a storage error reading the folder list; per-folder failures are
/// collected into the report.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip(db, capabilities, opts, cancel, connect, on_cycle, sink))]
pub async fn watch_folders<T, C, Fut, F, S>(
    db: &Database,
    account_id: i64,
    capabilities: ImapCapabilities,
    opts: IdleOptions,
    cancel: &CancellationToken,
    connect: C,
    on_cycle: F,
    sink: S,
) -> Result<AccountWatchReport, Error>
where
    T: ImapStream,
    C: Fn() -> Fut + Clone,
    Fut: std::future::Future<Output = Result<Session<T>, Error>>,
    F: Fn(WatchCycle) + Clone,
    S: ChangeSink + Clone,
{
    let mailboxes = db
        .read(move |c| repo::list_mailboxes(c, account_id))
        .await?;
    let watched: Vec<crate::repo::Mailbox> = super::full::prioritize(mailboxes)
        .into_iter()
        .take(opts.watch_limit.max(1))
        .collect();
    tracing::info!(
        folders = watched.len(),
        limit = opts.watch_limit,
        "watching account folders"
    );

    // A clone per watch: they run concurrently, so they cannot share one
    // exclusive borrow of the sink.
    let mut sinks: Vec<S> = watched.iter().map(|_| sink.clone()).collect();
    let running = watched.iter().zip(sinks.iter_mut()).map(|(mailbox, sink)| {
        watch_folder(
            db,
            mailbox.id,
            capabilities,
            opts,
            cancel,
            connect.clone(),
            on_cycle.clone(),
            sink,
        )
    });
    let results = futures::future::join_all(running).await;

    let mut out = AccountWatchReport::default();
    for (mailbox, result) in watched.into_iter().zip(results) {
        match result {
            Ok(report) => out.reports.push(report),
            Err(error) => {
                tracing::warn!(folder = %mailbox.name, %error, "folder cannot be watched");
                out.failures.push(WatchFailure {
                    mailbox_id: mailbox.id,
                    name: mailbox.name,
                    error,
                });
            }
        }
    }
    Ok(out)
}

/// What a park on `IDLE` ended with.
enum Parked<T: ImapStream> {
    /// The server spoke, or the re-IDLE cadence elapsed. The session is clean.
    Woken { session: Session<T>, pushed: bool },
    /// The watch was cancelled; the session was terminated cleanly and dropped.
    Cancelled,
    /// The connection broke. The session is gone.
    Lost(Error),
}

/// Park on `IDLE` until the server speaks, the cadence elapses, or the watch is
/// cancelled.
///
/// Consumes the session because async-imap's `IDLE` handle does; a clean exit
/// hands it back. Any error path drops it, which is correct — a session whose
/// `IDLE` did not terminate cleanly cannot be reused for anything else.
async fn park_on_idle<T: ImapStream>(
    session: Session<T>,
    opts: IdleOptions,
    cancel: &CancellationToken,
) -> Parked<T> {
    let re_idle = opts.effective_re_idle();
    let deadline = opts.sync.window_timeout;

    let mut handle = session.idle();
    // Every network wait here is bounded. `init` and `done` both read from the
    // socket with no deadline of their own, so a half-open connection — the
    // most ordinary way a long-lived link dies — would otherwise hang the watch
    // forever, including through cancellation.
    match tokio::time::timeout(deadline, handle.init()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Parked::Lost(super::command_error("IDLE", error)),
        Err(_) => {
            return Parked::Lost(Error::deadline_exceeded(format!(
                "IMAP IDLE was not acknowledged within {deadline:?}"
            )))
        }
    }

    let mut cancelled = false;
    let mut re_idled = false;
    // Scoped: the wait future borrows the handle, and `done()` below consumes
    // it. The borrow has to be over before that can happen.
    let response = {
        let (wait, interrupt) = handle.wait_with_timeout(re_idle);
        tokio::pin!(wait);
        // The cadence has to be enforced here, not by the argument above.
        // async-imap re-arms its own timeout on *every* response it decides to
        // ignore — and `* OK Still here`, which Dovecot, Cyrus and Gmail all
        // send every couple of minutes, is exactly such a response. Left to
        // itself the wait never returns on a healthy server, and the re-IDLE
        // this whole cadence exists for never happens.
        let cadence = tokio::time::sleep(re_idle);
        tokio::pin!(cadence);
        // Held until we want to interrupt: dropping the StopSource is what ends
        // the wait. The `if` guards disarm both arms afterwards so the loop
        // falls through to the (now resolving) wait rather than spinning.
        let mut interrupt = Some(interrupt);
        loop {
            tokio::select! {
                result = &mut wait => break result,
                () = &mut cadence, if interrupt.is_some() => {
                    tracing::debug!(?re_idle, "IDLE cadence elapsed; reissuing");
                    re_idled = true;
                    interrupt = None;
                }
                () = cancel.cancelled(), if interrupt.is_some() => {
                    tracing::debug!("watch cancelled; interrupting IDLE");
                    cancelled = true;
                    interrupt = None;
                }
            }
        }
    };

    let pushed = match response {
        Ok(IdleResponse::NewData(_)) => true,
        Ok(IdleResponse::Timeout | IdleResponse::ManualInterrupt) => false,
        Err(error) => return Parked::Lost(super::command_error("IDLE", error)),
    };
    let _ = re_idled;

    // Terminate the IDLE either way: leaving the server holding a command it
    // never saw finish is how connections get abandoned rather than closed.
    match tokio::time::timeout(deadline, handle.done()).await {
        Ok(Ok(session)) => {
            if cancelled {
                let _ = logout(session, deadline).await;
                Parked::Cancelled
            } else {
                Parked::Woken { session, pushed }
            }
        }
        Ok(Err(error)) => {
            if cancelled {
                Parked::Cancelled
            } else {
                Parked::Lost(super::command_error("IDLE DONE", error))
            }
        }
        Err(_) => {
            // The socket is not answering. Dropping the handle closes it, which
            // is the only thing left to do.
            if cancelled {
                Parked::Cancelled
            } else {
                Parked::Lost(Error::deadline_exceeded(format!(
                    "IMAP IDLE DONE was not acknowledged within {deadline:?}"
                )))
            }
        }
    }
}

/// Best-effort `LOGOUT`, bounded so a dead socket cannot delay a shutdown.
///
/// A watch that is stopping should close its connection rather than drop it:
/// the server otherwise holds the mailbox open until its own idle timeout.
async fn logout<T: ImapStream>(mut session: Session<T>, deadline: Duration) {
    let _ = tokio::time::timeout(deadline, session.logout()).await;
}
/// Whether a wait ended in cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waited {
    Elapsed,
    Cancelled,
}

impl Waited {
    fn is_cancelled(self) -> bool {
        self == Self::Cancelled
    }
}

/// Sleep for `duration`, returning early if the watch is cancelled.
async fn sleep_or_cancel(duration: Duration, cancel: &CancellationToken) -> Waited {
    tokio::select! {
        () = tokio::time::sleep(duration) => Waited::Elapsed,
        () = cancel.cancelled() => Waited::Cancelled,
    }
}

#[cfg(test)]
mod tests;
