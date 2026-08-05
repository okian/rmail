//! The [`Scope`] capability model.
//!
//! A scope is a single, independently-grantable capability — `mail.read`,
//! `admin`, and so on. A token (or the implicit Unix-peer-uid principal)
//! carries a set of them; the daemon's auth layer (`rmaild::auth`) checks a
//! per-method requirement against that set via [`satisfies`]. The string form
//! is a wire/storage contract: it is what `api_tokens.scopes` persists, what
//! `AdminService.MintToken` accepts, and what `mail token create --scope`
//! takes on the command line, so [`Scope::as_wire`]/[`std::str::FromStr`] stay
//! in lock-step and are exercised by round-trip tests.

use std::fmt;
use std::str::FromStr;

/// A single capability grant.
///
/// Deliberately *not* `Copy`: [`Scope::Mailbox`] carries an owned folder name,
/// and a type that is `Copy` for every variant but one invites a caller to
/// write `.clone()`-free code that then fails to compile the day a second
/// string-carrying variant is added — better that every call site already
/// clones explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Read mail: list/get messages, threads, attachments; watch events.
    MailRead,
    /// Mutate mail: move/copy/flag/delete.
    MailWrite,
    /// Send mail (compose/outbox).
    MailSend,
    /// Invoke AI features (summarize, draft, ask-mailbox, ...).
    AiInvoke,
    /// Spend up to a whole-dollar cap on AI provider calls in a budget window.
    ///
    /// A granted `AiSpend(cap)` satisfies a required `AiSpend(need)` when
    /// `cap >= need` — see [`satisfies`] — so a token minted with a $5 cap
    /// cannot be used to authorize a single call that would need $50.
    ///
    /// **Not yet enforced.** `MintToken` accepts and stores it, but no method
    /// in `rmaild::auth::methods`'s table requires `AiSpend` today — that
    /// table is keyed by method name alone and cannot see a request's dollar
    /// amount. Real enforcement needs a per-request budget check inside the
    /// AI handlers themselves (tasks 45/76); until then this scope is inert,
    /// not a lever an operator can rely on to cap spend.
    AiSpend(u32),
    /// Restricted to one named mailbox/folder.
    ///
    /// **Not yet enforced**, for the same structural reason as [`Scope::AiSpend`]:
    /// the per-method scope table has no way to see *which* mailbox a request
    /// names. A token minted with only `Mailbox("INBOX")` currently grants
    /// nothing at all (no method in the table requires it), which is
    /// fail-safe but easy to mistake for "restricted to INBOX" — it is not.
    /// Real enforcement is a resource-level check inside `MailService`'s
    /// handlers (task 39).
    Mailbox(String),
    /// Rules/hooks/webhooks automation surfaces.
    Automation,
    /// Every capability, unconditionally. The scope the Unix-peer-uid path
    /// grants implicitly, and the only one `MintToken`/`RevokeToken`/
    /// `ListTokens` accept.
    Admin,
}

impl Scope {
    /// The wire/storage string for this scope (round-trips through
    /// [`FromStr`]).
    #[must_use]
    pub fn as_wire(&self) -> String {
        match self {
            Scope::MailRead => "mail.read".to_owned(),
            Scope::MailWrite => "mail.write".to_owned(),
            Scope::MailSend => "mail.send".to_owned(),
            Scope::AiInvoke => "ai.invoke".to_owned(),
            Scope::AiSpend(cap) => format!("ai.spend:{cap}"),
            Scope::Mailbox(name) => format!("mailbox:{name}"),
            Scope::Automation => "automation".to_owned(),
            Scope::Admin => "admin".to_owned(),
        }
    }

    /// Join a scope set into `api_tokens.scopes`' comma-separated storage form.
    #[must_use]
    pub fn join(scopes: &[Scope]) -> String {
        scopes
            .iter()
            .map(Scope::as_wire)
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Parse a comma-separated scope list (the storage/`MintToken` form).
    ///
    /// # Errors
    ///
    /// [`ScopeParseError`] naming the first token that does not parse.
    /// Trailing/interior empty segments (e.g. `"mail.read,,admin"`) are
    /// rejected rather than silently skipped, since a malformed list here
    /// most likely means a scope the caller intended is missing.
    pub fn parse_list(joined: &str) -> Result<Vec<Scope>, ScopeParseError> {
        joined.split(',').map(str::parse).collect()
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_wire())
    }
}

/// A scope string did not match any known form.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid scope {0:?}")]
pub struct ScopeParseError(pub String);

impl FromStr for Scope {
    type Err = ScopeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mail.read" => Ok(Scope::MailRead),
            "mail.write" => Ok(Scope::MailWrite),
            "mail.send" => Ok(Scope::MailSend),
            "ai.invoke" => Ok(Scope::AiInvoke),
            "automation" => Ok(Scope::Automation),
            "admin" => Ok(Scope::Admin),
            _ => {
                if let Some(rest) = s.strip_prefix("ai.spend:") {
                    rest.parse::<u32>()
                        .map(Scope::AiSpend)
                        .map_err(|_| ScopeParseError(s.to_owned()))
                } else if let Some(rest) = s.strip_prefix("mailbox:") {
                    if rest.is_empty() {
                        Err(ScopeParseError(s.to_owned()))
                    } else {
                        Ok(Scope::Mailbox(rest.to_owned()))
                    }
                } else {
                    Err(ScopeParseError(s.to_owned()))
                }
            }
        }
    }
}

/// Whether a set of granted scopes satisfies a required capability.
///
/// [`Scope::Admin`] is a superset of every other scope — the entire point of
/// the Unix-peer-uid path granting it implicitly is that a trusted local
/// caller can do anything without enumerating every capability by hand. Every
/// other pair must match by variant; [`Scope::AiSpend`] additionally requires
/// the granted cap to be at least the required one.
#[must_use]
pub fn satisfies(granted: &[Scope], required: &Scope) -> bool {
    granted
        .iter()
        .any(|g| matches!(g, Scope::Admin) || scope_covers(g, required))
}

/// Whether `granted` alone (ignoring [`Scope::Admin`]) covers `required`.
fn scope_covers(granted: &Scope, required: &Scope) -> bool {
    match (granted, required) {
        (Scope::AiSpend(cap), Scope::AiSpend(need)) => cap >= need,
        (a, b) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_simple_scope_round_trips_through_its_wire_string() {
        for (scope, wire) in [
            (Scope::MailRead, "mail.read"),
            (Scope::MailWrite, "mail.write"),
            (Scope::MailSend, "mail.send"),
            (Scope::AiInvoke, "ai.invoke"),
            (Scope::Automation, "automation"),
            (Scope::Admin, "admin"),
        ] {
            assert_eq!(scope.as_wire(), wire);
            assert_eq!(wire.parse::<Scope>().unwrap(), scope);
        }
    }

    #[test]
    fn ai_spend_and_mailbox_carry_their_parameter_through_the_wire_form() {
        assert_eq!(Scope::AiSpend(5).as_wire(), "ai.spend:5");
        assert_eq!("ai.spend:5".parse::<Scope>().unwrap(), Scope::AiSpend(5));

        assert_eq!(
            Scope::Mailbox("INBOX".to_owned()).as_wire(),
            "mailbox:INBOX"
        );
        assert_eq!(
            "mailbox:INBOX".parse::<Scope>().unwrap(),
            Scope::Mailbox("INBOX".to_owned())
        );
    }

    #[test]
    fn unknown_and_malformed_strings_are_rejected() {
        for bad in [
            "",
            "mail.delete",
            "ai.spend:",
            "ai.spend:notanumber",
            "mailbox:",
            "ADMIN",
        ] {
            assert!(bad.parse::<Scope>().is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn join_and_parse_list_round_trip() {
        let scopes = vec![Scope::MailRead, Scope::MailSend, Scope::AiSpend(5)];
        let joined = Scope::join(&scopes);
        assert_eq!(joined, "mail.read,mail.send,ai.spend:5");
        assert_eq!(Scope::parse_list(&joined).unwrap(), scopes);
    }

    #[test]
    fn an_empty_segment_in_a_scope_list_is_rejected_not_skipped() {
        // A malformed list most likely means a scope the caller intended is
        // missing — skipping the empty segment silently would mint a token
        // with fewer capabilities than asked for and no error to explain why.
        assert!(Scope::parse_list("mail.read,,admin").is_err());
    }

    #[test]
    fn admin_satisfies_every_requirement() {
        let granted = vec![Scope::Admin];
        for required in [
            Scope::MailRead,
            Scope::MailWrite,
            Scope::MailSend,
            Scope::AiInvoke,
            Scope::AiSpend(1_000_000),
            Scope::Mailbox("Archive".to_owned()),
            Scope::Automation,
            Scope::Admin,
        ] {
            assert!(
                satisfies(&granted, &required),
                "admin should satisfy {required:?}"
            );
        }
    }

    #[test]
    fn a_read_only_grant_does_not_satisfy_write_or_send() {
        let granted = vec![Scope::MailRead];
        assert!(satisfies(&granted, &Scope::MailRead));
        assert!(!satisfies(&granted, &Scope::MailWrite));
        assert!(!satisfies(&granted, &Scope::MailSend));
        assert!(!satisfies(&granted, &Scope::Admin));
    }

    #[test]
    fn ai_spend_requires_the_granted_cap_to_cover_the_requirement() {
        let granted = vec![Scope::AiSpend(5)];
        assert!(satisfies(&granted, &Scope::AiSpend(5)));
        assert!(satisfies(&granted, &Scope::AiSpend(1)));
        assert!(!satisfies(&granted, &Scope::AiSpend(50)));
    }

    #[test]
    fn an_empty_grant_set_satisfies_nothing() {
        assert!(!satisfies(&[], &Scope::MailRead));
        assert!(!satisfies(&[], &Scope::Admin));
    }
}
