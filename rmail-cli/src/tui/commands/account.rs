//! The account verbs (task 97): which accounts exist, which one this session is
//! looking at, and the three ways one gets its credentials.
//!
//! # `:account add` proposes; `:account new` writes
//!
//! Two verbs because they are two different acts, and one verb switching between
//! them on the presence of a flag would make which one ran depend on a detail
//! nobody would think to check.
//!
//! `Autoconfigure` is explicit that it "writes no account, touches no existing
//! one, and returns a proposal for a human to apply" — and on a miss the
//! proposal can come from a model. So `:account add ada@example.com` probes and
//! reports; nothing is stored. The report carries the ready `[[accounts]]` TOML
//! block for `rmail.toml` *and* a row whose `<enter>` applies the proposal
//! through `Create`, which is the other way to apply it.
//!
//! That row is why `:account new` has to be a verb at all: a row's action *is*
//! an `Invocation` (task 90), so applying a proposal means there is a `:` line
//! that applies it — and `wire::autoconfigure_rows` builds exactly that line,
//! flag by flag, from what was discovered. `new` rather than `create` to sit
//! next to `:tag new`, which is `CreateTag`; `AccountCreate` has no CLI verb at
//! all, so there was no spelling to inherit.
//!
//! # `:account use` is the one verb here that reaches nothing
//!
//! Switching which account is on screen is local: no RPC changes, and the
//! screen's folders, message list and open message all belong to the account
//! they came from. `tui::model`'s `run_invocation` answers it next to `:set`,
//! for the reason that function's own comment gives — the id it takes is not
//! something an `Action` can carry.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::{first, flag, id_positional, switch, Answer, Request, Target};
use crate::tui::model::{Cmd, Credential};
use crate::tui::report::ReportColumn;

/// The columns `:account list` draws.
fn account_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("id", 5),
        ReportColumn::new("account", 24),
        ReportColumn::new("login", 26),
        ReportColumn::new("imap", 28),
        ReportColumn::new("credential", 22),
    ]
}

/// The columns every one-account detail table draws — `:account show`,
/// `:account test`, the OAuth flow, a refresh.
///
/// One shape for all four, for the reason the rule verbs share `outcome_columns`:
/// they answer with fields about one account, and four layouts over one subject
/// would be four chances to disagree about what a column means.
pub(super) fn field_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("what", 20),
        ReportColumn::new("value", 56),
    ]
}

/// The account verbs' answers.
#[must_use]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "account list" => Request::rows(
            Cmd::AccountList {
                generation,
                open: target.account_id,
            },
            "accounts — :account use <id> switches",
            account_columns(),
        ),
        "account show" => {
            let account_id = match which(invocation, target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AccountShow {
                    generation,
                    account_id,
                },
                &format!("account {account_id}"),
                field_columns(),
            )
        }
        "account add" => {
            let Some(email) = first(invocation) else {
                return Some(Answer::Refused(
                    "name an address to configure — :account add you@example.com".to_owned(),
                ));
            };
            let credential = match credential(invocation) {
                Ok(credential) => credential,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AccountDiscover {
                    generation,
                    email: email.clone(),
                    credential,
                    // Off unless asked: it costs money, and a proposal is a
                    // guess even after the daemon validates it.
                    allow_model: switch(invocation, "ai"),
                },
                &format!("account add {email} — a proposal, nothing stored"),
                field_columns(),
            )
        }
        "account new" => {
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "name the account — :account add <address> discovers the rest".to_owned(),
                ));
            };
            let settings = match settings(invocation) {
                Ok(settings) => settings,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::fact(
                Cmd::AccountCreate {
                    name: name.clone(),
                    settings,
                },
                "storing the account…",
            )
        }
        "account login" => {
            let account_id = match which(invocation, target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let Some(provider) = flag(invocation, "oauth") else {
                return Some(Answer::Refused(
                    "which provider — :account login --oauth=google --client-id=…".to_owned(),
                ));
            };
            let Some(client_id) = flag(invocation, "client-id") else {
                return Some(Answer::Refused(
                    "--client-id is the id of a native app you registered with the provider"
                        .to_owned(),
                ));
            };
            Request::rows(
                Cmd::AccountLogin {
                    generation,
                    account_id,
                    provider: provider.to_owned(),
                    client_id: client_id.to_owned(),
                    client_secret_command: flag(invocation, "client-secret-command")
                        .map(str::to_owned),
                    scopes: scopes(invocation),
                    // The URL is on screen either way; `--no-browser` is for a
                    // machine with no handler, or for somebody who would rather
                    // paste it themselves.
                    open_browser: !switch(invocation, "no-browser"),
                },
                &format!("account login {account_id} — waiting for the browser"),
                field_columns(),
            )
        }
        "account refresh" => {
            let account_id = match which(invocation, target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AccountRefresh {
                    generation,
                    account_id,
                    force: switch(invocation, "force"),
                },
                &format!("account refresh {account_id}"),
                field_columns(),
            )
        }
        "account test" => {
            let account_id = match which(invocation, target) {
                Ok(account_id) => account_id,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::AccountTest {
                    generation,
                    account_id,
                },
                &format!("account test {account_id} — logging in"),
                field_columns(),
            )
        }
        "account rm" => {
            let Some(account_id) = id_positional(invocation) else {
                return Some(Answer::Refused(
                    "which account — :account list has the ids".to_owned(),
                ));
            };
            // The one verb here that asks. Deleting an account cascades to its
            // mailboxes and its stored mail, so it is expensive and hard to
            // undo — which is the judgement `Request::confirm` is for, and not
            // the same question as "does it mutate" (task 89 settled that a
            // typed `:` line is already the deliberate act).
            //
            // Deliberately *not* defaulted from the screen either: every other
            // account verb falls back to the account on screen, and a `:` line
            // that deleted whatever happened to be open because its id was left
            // off is a line nobody should be able to type by accident.
            Answer::Rows(Box::new(Request {
                cmd: Cmd::AccountDelete { account_id },
                title: format!("account rm {account_id}"),
                columns: field_columns(),
                confirm: Some(format!(
                    "delete account {account_id} and every message stored for it? [y/N]"
                )),
                // Re-runnable: the report is what came back, and `r` on it asks
                // the *delete* again — which is idempotent here and reports the
                // same answer. Nothing is produced that a second run duplicates.
                once: false,
            }))
        }
        _ => return None,
    })
}

/// Which account a verb acts on: the id it was given, or the one on screen.
///
/// Falling back is right for a read and for a login — "this account" is what
/// somebody means when they are looking at it — and deliberately not offered for
/// `:account rm`, which is the one verb where guessing is unrecoverable.
///
/// # Errors
///
/// A message naming the listing that has the ids, when neither is available.
fn which(invocation: &Invocation, target: &Target) -> Result<i64, String> {
    if let Some(account_id) = id_positional(invocation) {
        return Ok(account_id);
    }
    if !invocation.positionals.is_empty() {
        return Err(format!(
            "{:?} is not an account id — :account list has them",
            invocation.positionals.join(" ")
        ));
    }
    match target.account_id {
        0 => Err("no account on screen — give an id, or see :account list".to_owned()),
        account_id => Ok(account_id),
    }
}

/// The credential *reference* a line carried, if it carried one.
///
/// At most one: the proto's `CredentialRef` is a oneof, and a request carrying
/// two would have one of them silently dropped at the wire seam. Refused here
/// instead, where the line that named both is still on screen.
///
/// # Errors
///
/// A message naming the two flags, when more than one was given.
fn credential(invocation: &Invocation) -> Result<Option<Credential>, String> {
    let mut found: Vec<(&str, Credential)> = Vec::new();
    for (name, wrap) in [
        ("password-command", Credential::Command as fn(String) -> _),
        ("password-env", Credential::Env),
        ("keychain", Credential::Keychain),
        ("oauth", Credential::OAuth),
    ] {
        if let Some(value) = flag(invocation, name) {
            found.push((name, wrap(value.to_owned())));
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop().map(|(_, credential)| credential)),
        _ => Err(format!(
            "one credential at a time — {} were given",
            found
                .iter()
                .map(|(name, _)| format!("--{name}"))
                .collect::<Vec<_>>()
                .join(" and ")
        )),
    }
}

/// The server settings a `:account new` line carried, as text.
///
/// Text rather than numbers for the reason the budget caps are: the wire seam is
/// the one place a port becomes a `u32`, and parsing here and again there would
/// be two validators to keep in step. What *is* checked here is that a port is a
/// port at all, so a typo is refused where it was typed.
///
/// # Errors
///
/// A message naming the offending flag, or the two credentials.
fn settings(invocation: &Invocation) -> Result<Vec<(String, String)>, String> {
    // Checked for its own sake: `Cmd::AccountCreate` carries the pairs, and a
    // line naming two credentials would reach the wire seam with one of them
    // dropped.
    credential(invocation)?;
    let mut settings = Vec::new();
    for name in [
        "imap-server",
        "imap-port",
        "username",
        "smtp-server",
        "smtp-port",
        "password-command",
        "password-env",
        "keychain",
        "oauth",
    ] {
        let Some(value) = flag(invocation, name) else {
            continue;
        };
        if name.ends_with("port") {
            let port: u32 = value
                .parse()
                .ok()
                .filter(|port| (1..=65535).contains(port))
                .ok_or_else(|| format!("--{name} {value:?}: a port, 1 to 65535"))?;
            settings.push((name.to_owned(), port.to_string()));
            continue;
        }
        settings.push((name.to_owned(), value.to_owned()));
    }
    Ok(settings)
}

/// Every `--scope` a line carried, comma-separated values included.
///
/// Both spellings, because `mail account login --scope a,b` and
/// `--scope a --scope b` are both accepted there and a TUI that took only one of
/// them would be the surface where a documented form did nothing.
pub(super) fn scopes(invocation: &Invocation) -> Vec<String> {
    invocation
        .flags
        .iter()
        .filter(|flag| flag.name == "scope")
        .filter_map(|flag| flag.value.as_deref())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect()
}
