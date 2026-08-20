//! The capability-token verbs (task 97): what exists, minting one, revoking one.
//!
//! # The secret is shown once, and once means once
//!
//! `MintToken` returns the bearer secret in its response and nowhere else — only
//! an argon2id hash is persisted, so the daemon *cannot* show it again. A client
//! that lost it has lost it, and the only remedy is revoke-and-mint.
//!
//! That shapes three decisions here. The secret goes into the Report's rows and
//! into nothing else: not the status line, not the command history (the line
//! `:token create --name ci --scope admin` carries no secret, which is why the
//! label and the scopes are flags rather than the secret being echoed back). The
//! row says outright that it cannot be shown again, because a reader who does not
//! know that will close the pane. And [`Request::once`] marks the report
//! un-re-runnable: `r` means "ask this verb again", and asking `MintToken` again
//! is minting a second token — a report whose verb *produced* something is not a
//! report that can be refreshed.
//!
//! # Metadata only, everywhere else
//!
//! `ListTokens` never returns the secret or its hash, which is why `:token list`
//! can be an ordinary re-runnable report.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::account::scopes;
use super::{flag, id_positional, Answer, Request, Target};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// The token verbs' answers.
#[must_use]
pub fn answer(invocation: &Invocation, _target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "token list" => Request::rows(
            Cmd::TokenList { generation },
            "tokens — metadata only, never a secret",
            vec![
                ReportColumn::new("id", 5),
                ReportColumn::new("name", 20),
                ReportColumn::new("state", 10),
                ReportColumn::new("last used", 17),
                ReportColumn::new("expires", 17),
                ReportColumn::new("scopes", 30),
            ],
        ),
        "token create" => {
            let Some(name) = flag(invocation, "name") else {
                return Some(Answer::Refused(
                    "label it — :token create --name=ci --scope=mail.read".to_owned(),
                ));
            };
            let scopes = scopes(invocation);
            if scopes.is_empty() {
                // The daemon refuses this too, and saying so now is the same
                // answer sooner: a token minted with no scopes could never do
                // anything, which is almost certainly not what was meant.
                return Some(Answer::Refused(
                    "at least one --scope — mail.read, mail.write, mail.send, ai.invoke, \
                     automation, admin"
                        .to_owned(),
                ));
            }
            let ttl_secs = match ttl(invocation) {
                Ok(ttl_secs) => ttl_secs,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Answer::Rows(Box::new(Request {
                cmd: Cmd::TokenCreate {
                    generation,
                    name: name.to_owned(),
                    scopes,
                    ttl_secs,
                },
                title: format!("token create {name} — the secret is shown once"),
                columns: vec![
                    ReportColumn::new("what", 12),
                    ReportColumn::new("value", 64),
                ],
                confirm: None,
                // `r` on this report would mint a second token. See the module
                // docs.
                once: true,
            }))
        }
        "token revoke" => {
            let Some(token_id) = id_positional(invocation) else {
                return Some(Answer::Refused(
                    "which token — :token list has the ids".to_owned(),
                ));
            };
            // No confirmation, unlike `:account rm`: revoking is the *safe*
            // direction — nothing is lost that was not already unrecoverable,
            // and re-revoking is explicitly not an error on this RPC. Asking
            // here would be asking hardest about the answer somebody reaching
            // for it in a hurry needs.
            Request::fact(Cmd::TokenRevoke { token_id }, "revoking…")
        }
        _ => return None,
    })
}

/// How long a minted token lives, in seconds from now.
///
/// `rmail_core::config::parse_human_duration` is what `mail token create --ttl`
/// uses, so `24h` and `90d` mean here what they mean there — a second duration
/// grammar for one flag is the drift `parity` exists to prevent.
///
/// # Errors
///
/// A message naming the offending value.
fn ttl(invocation: &Invocation) -> Result<Option<i64>, String> {
    let Some(text) = flag(invocation, "ttl") else {
        return Ok(None);
    };
    let duration = rmail_core::config::parse_human_duration(text)
        .map_err(|error| format!("--ttl {text:?}: {error}"))?;
    let secs = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    if secs <= 0 {
        // Zero is `INVALID_ARGUMENT` at the daemon and is explicitly not an
        // alternate spelling of "never expires" — omitting `--ttl` is.
        return Err(format!(
            "--ttl {text:?}: a positive duration, or leave it off for no expiry"
        ));
    }
    Ok(Some(secs))
}
