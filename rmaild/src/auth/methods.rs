//! The declarative table of gRPC method -> required capability.
//!
//! Keyed by the fully-qualified gRPC method path (`req.uri().path()`, e.g.
//! `/rmail.v1.AccountService/Create`) rather than by generated service/method
//! types, because that string is exactly what the auth layer has on hand
//! *before* any service-specific decoding happens — matching it is the whole
//! point of enforcing this ahead of dispatch, not inside each handler.
//!
//! # Fail closed
//!
//! [`lookup`] returns `None` for a method with no row here, and callers must
//! treat `None` as **deny**, not allow. The alternative — defaulting an
//! unregistered RPC to public — means a service wired up without remembering
//! to add a row here is silently wide open; failing closed turns the same
//! mistake into a `PERMISSION_DENIED` the first time anything calls it.
//!
//! # Extending this table
//!
//! Add one `(method, Requirement)` row per new RPC below. `OutboxService/Send`
//! is still **provisional** — despite sitting in the `MailService` section
//! below, next to `Send`'s acceptance case, no `OutboxService` proto exists
//! yet (not yet assigned to a task), so its row exists only to prove the
//! table's *mechanism* — including the acceptance case that a
//! `mail.read`-only token is physically denied a send/delete-shaped call —
//! against a shape close to what it will actually need. When a real service
//! lands, treat its rows as a starting point to confirm against the real
//! proto, not as settled fact: rename/add/remove rather than assuming these
//! are exactly right.
//!
//! The `MailService` rows below were provisional the same way until task 39
//! landed the real `proto/rmail/v1/mail.proto`; they turned out to need no
//! changes — every RPC the real service exposes (`List`, `Get`, `GetThread`,
//! `Move`, `Copy`, `SetFlags`, `Delete`, `GetAttachment`, `WatchEvents`) is
//! named here with the scope its handler actually needs. The `AiService` rows
//! were provisional the same way until task 50 landed the real
//! `proto/rmail/v1/ai.proto`; unlike `MailService`, they *did* need to change
//! — the provisional `Summarize`/`AskMailbox` guesses matched neither the real
//! service's RPC names nor, in `AskMailbox`'s case, this service at all (that
//! one belongs to task 52) — see the `AiService` section below for the real
//! six rows and their scope justifications.
use rmail_core::auth::Scope;

/// What a method needs from the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    /// No authentication or scope required (health, reflection).
    Public,
    /// The caller's granted scopes must satisfy this one
    /// (see [`rmail_core::auth::satisfies`]).
    Scope(Scope),
}

/// method path -> requirement. See the module docs for the fail-closed
/// contract and the provisional-rows note.
const TABLE: &[(&str, Requirement)] = &[
    // -- Cross-cutting, always public --------------------------------------
    ("/grpc.health.v1.Health/Check", Requirement::Public),
    ("/grpc.health.v1.Health/Watch", Requirement::Public),
    (
        "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo",
        Requirement::Public,
    ),
    // -- AccountService (task 7) --------------------------------------------
    // Account rows hold IMAP/SMTP host+credential configuration; creating,
    // deleting, or exercising a login (`TestConnection`) is account
    // *management*, not mail content, so it sits behind `admin` rather than
    // `mail.write`. Reading the (secret-free) list/get view only needs
    // `mail.read`, since most read-only automation needs to know which
    // accounts exist to do anything useful with them.
    (
        "/rmail.v1.AccountService/Create",
        Requirement::Scope(Scope::Admin),
    ),
    (
        "/rmail.v1.AccountService/List",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.AccountService/Get",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.AccountService/Delete",
        Requirement::Scope(Scope::Admin),
    ),
    (
        "/rmail.v1.AccountService/TestConnection",
        Requirement::Scope(Scope::Admin),
    ),
    // -- SyncService (task 15) -----------------------------------------------
    // Triggering/pausing/resuming a sync mutates local state (and drives IMAP
    // traffic), so it needs `mail.write`; observing status/events is `mail.read`.
    (
        "/rmail.v1.SyncService/SyncFolder",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SyncService/Status",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SyncService/Pause",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SyncService/Resume",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SyncService/WatchEvents",
        Requirement::Scope(Scope::MailRead),
    ),
    // -- AdminService (task 38) ----------------------------------------------
    // Token lifecycle is inherently an admin action: minting a token *creates*
    // capability, so anything less than `admin` would let a token mint a
    // sibling with scopes of its own choosing.
    (
        "/rmail.v1.AdminService/MintToken",
        Requirement::Scope(Scope::Admin),
    ),
    (
        "/rmail.v1.AdminService/RevokeToken",
        Requirement::Scope(Scope::Admin),
    ),
    (
        "/rmail.v1.AdminService/ListTokens",
        Requirement::Scope(Scope::Admin),
    ),
    // -- AuditService (task 45) ----------------------------------------------
    // The ledger is the record of what was sent to a model provider, including
    // an account id and a message id per call. Reading it is therefore reading
    // metadata about mail, and `admin` rather than `mail.read` because the
    // trail exists to hold the operator to account: a token minted for routine
    // mail access should not be able to enumerate — or export wholesale — the
    // history of every AI call made on this machine.
    (
        "/rmail.v1.AuditService/QueryAiCalls",
        Requirement::Scope(Scope::Admin),
    ),
    (
        "/rmail.v1.AuditService/ExportLedger",
        Requirement::Scope(Scope::Admin),
    ),
    // -- MailService (task 39) -----------------------------------------------
    // Reads (list/get/thread/attachment/watch) are local-mirror lookups, so
    // `mail.read` suffices; every mutation reflects to the live IMAP server
    // (see rmail-core::mail's module docs), so those sit behind `mail.write`.
    (
        "/rmail.v1.MailService/List",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.MailService/Get",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.MailService/GetThread",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.MailService/GetAttachment",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.MailService/WatchEvents",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.MailService/Move",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.MailService/Copy",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.MailService/SetFlags",
        Requirement::Scope(Scope::MailWrite),
    ),
    // The acceptance case: a `mail.read`-only token must be physically denied
    // this one (and `OutboxService/Send`, below) — see `auth::tests`.
    (
        "/rmail.v1.MailService/Delete",
        Requirement::Scope(Scope::MailWrite),
    ),
    // -- NoteService (task 56) -------------------------------------------------
    // Notes are local mail annotations, scoped the same way `MailService`'s
    // own reads/mutations are: `ListNotes`/`WatchNotes` read the local
    // database only, so `mail.read` suffices; `AddNote`/`EditNote`/
    // `DeleteNote` mutate it, so they sit behind `mail.write` — the same
    // read/write split this table already draws for `MailService.List` vs
    // `MailService.SetFlags`. Nothing here reaches IMAP or a model provider,
    // so there is no reason for any row to require more than `mail.write`.
    (
        "/rmail.v1.NoteService/AddNote",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.NoteService/EditNote",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.NoteService/DeleteNote",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.NoteService/ListNotes",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.NoteService/WatchNotes",
        Requirement::Scope(Scope::MailRead),
    ),
    // -- SearchService (task 33) ----------------------------------------------
    // Every RPC here is a read-only query over the local index (no IMAP round
    // trip, no mutation — see `search_service`'s own module docs), so
    // `mail.read` is the right ceiling for all three: `Search`/`Semantic`
    // rank and stream messages the caller could already `MailService::List`,
    // and `Explain` only re-derives a rationale for one, already-visible
    // message. None of the three needs `mail.write`.
    (
        "/rmail.v1.SearchService/Search",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SearchService/Semantic",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SearchService/Explain",
        Requirement::Scope(Scope::MailRead),
    ),
    // -- OutboxService (provisional; no task owns it yet) ---------------------
    // Still a forward declaration, unlike the `MailService` rows above it:
    // no `OutboxService` proto exists. Kept here (rather than filed with
    // `AiService` below) because it is part of the same acceptance case —
    // a `mail.read`-only token must be denied a send, not just a delete.
    (
        "/rmail.v1.OutboxService/Send",
        Requirement::Scope(Scope::MailSend),
    ),
    // -- AiService (task 50) --------------------------------------------------
    // The provisional `Summarize`/`AskMailbox` rows this table carried before
    // task 50 landed the real `proto/rmail/v1/ai.proto` did not survive
    // contact with it — neither method exists on the real service (`AskMailbox`
    // is task 52's "Mailbox RAG `ask_mailbox`", not this one) — replaced here
    // with the six RPCs `AiService` actually exposes, per this table's own
    // "confirm against the real proto... rename/add/remove" note above.
    //
    // GetSummary/StreamEnrichments never call the model — they read
    // `ai_summaries` exactly as cached, the same local-mirror-read shape
    // `MailService`'s reads have, so `mail.read` is the right ceiling: a
    // routine mail-reading token can see what the AI already produced without
    // being able to spend anything new.
    (
        "/rmail.v1.AiService/GetSummary",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.AiService/StreamEnrichments",
        Requirement::Scope(Scope::MailRead),
    ),
    // AnalyzeMessage/SuggestReply are the two RPCs that can actually call the
    // provider (a forced, on-demand deep pass — see `rmaild::ai_service`'s own
    // module docs) — `ai.invoke` is exactly the scope this table's own header
    // note anticipated for "the analyze/reply paths."
    (
        "/rmail.v1.AiService/AnalyzeMessage",
        Requirement::Scope(Scope::AiInvoke),
    ),
    (
        "/rmail.v1.AiService/SuggestReply",
        Requirement::Scope(Scope::AiInvoke),
    ),
    // GetUsage is arguably `mail.read` (it is, after all, a read), but it is
    // scoped `admin` instead: unlike GetSummary/StreamEnrichments, which
    // answer about one message a caller already named, GetUsage exposes
    // *aggregate spend* — the same "the trail exists to hold the operator to
    // account, not for routine mail access to enumerate" reasoning
    // `AuditService.QueryAiCalls`'s row above gives for the identical
    // question at the per-call level. A token minted to summarize messages
    // should not also be able to read the account's total AI dollar spend.
    (
        "/rmail.v1.AiService/GetUsage",
        Requirement::Scope(Scope::Admin),
    ),
    // SetPaused is a daemon-wide control-plane toggle — it does not name an
    // account or a message, it turns the *entire* AI pipeline on or off for
    // every account this daemon serves. That is squarely the same class of
    // action `AccountService.Create`/`Delete` sit behind `admin` for: it
    // mutates shared, global state a token minted for one caller's own AI use
    // should not be able to disable (denying every other caller's AI
    // features) or silently re-enable out from under a deliberate pause.
    (
        "/rmail.v1.AiService/SetPaused",
        Requirement::Scope(Scope::Admin),
    ),
    // RetryFailed is scoped like SetPaused, not AnalyzeMessage/SuggestReply,
    // despite reading like a request to do more work: `revive_all_dead`
    // (rmail_core::ai::AiQueue) is un-scoped by message *or account* — it
    // requeues every quarantined job across the whole daemon, causing spend
    // on accounts the calling token may have nothing to do with. That is the
    // same "mutates shared, global state" test SetPaused's row above applies,
    // not "asks the pipeline to attempt work for a message I named."
    (
        "/rmail.v1.AiService/RetryFailed",
        Requirement::Scope(Scope::Admin),
    ),
];

/// The requirement for `method` (a full gRPC path like
/// `/rmail.v1.AccountService/Create`), or `None` if the method is
/// unregistered. Callers must treat `None` as deny — see the module docs.
#[must_use]
pub fn lookup(method: &str) -> Option<&'static Requirement> {
    TABLE.iter().find(|(m, _)| *m == method).map(|(_, r)| r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_is_registered_exactly_once() {
        let mut seen = std::collections::HashSet::new();
        for (method, _) in TABLE {
            assert!(seen.insert(*method), "duplicate row for {method}");
        }
    }

    #[test]
    fn lookup_finds_every_registered_method() {
        for (method, requirement) in TABLE {
            assert_eq!(lookup(method), Some(requirement));
        }
    }

    #[test]
    fn an_unregistered_method_is_not_found_which_callers_must_deny() {
        assert_eq!(lookup("/rmail.v1.DoesNotExist/Method"), None);
    }

    /// Every method the server actually exposes has a row.
    ///
    /// The rest of the tests here check this table against itself, which cannot
    /// catch the failure that matters: a service is added, nobody adds its rows,
    /// and because [`lookup`] fails closed every one of its RPCs is denied at
    /// runtime with no compile-time or test-time complaint. That is exactly what
    /// happened when `AuditService` landed — it was written against a checkout
    /// that predated this table, so it shipped deny-everything.
    ///
    /// Reconciling against the compiled descriptor set is the only check that
    /// scales: the descriptor is generated from the protos, so a new RPC appears
    /// here the moment it exists, whether or not anyone remembered this file.
    #[test]
    fn every_rpc_in_the_descriptor_set_has_a_scope_row() {
        for (service, method) in descriptor_methods() {
            let path = format!("/{service}/{method}");
            assert!(
                lookup(&path).is_some(),
                "{path} is served but has no row in the scope table, so the \
                 fail-closed default denies every call to it. Add a row."
            );
        }
    }

    /// No row names a method of an existing service that does not exist.
    ///
    /// Rows for a service absent from the descriptor set are allowed on purpose:
    /// scopes are written ahead of the services they will govern, so a task can
    /// land its RPCs into a table that already expects them. But once a service
    /// *is* compiled in, a row naming a method it does not have is a typo — and
    /// a silent one, since the row simply never matches while the real method
    /// falls through to the deny default.
    #[test]
    fn no_row_names_a_missing_method_of_a_service_that_exists() {
        let methods = descriptor_methods();
        let served: std::collections::HashSet<String> =
            methods.iter().map(|(s, m)| format!("/{s}/{m}")).collect();
        let services: std::collections::HashSet<&str> =
            methods.iter().map(|(s, _)| s.as_str()).collect();

        for (path, _) in TABLE {
            let Some(service) = path.strip_prefix('/').and_then(|p| p.split('/').next()) else {
                continue;
            };
            if !services.contains(service) {
                // The service has not landed yet; the row is a forward
                // declaration, which is allowed.
                continue;
            }
            assert!(
                served.contains(*path),
                "{path} names a method that {service} does not have — the row \
                 never matches, and the real method is denied by default."
            );
        }
    }

    /// Every `(fully.qualified.Service, Method)` pair in the compiled protos.
    fn descriptor_methods() -> Vec<(String, String)> {
        use prost::Message as _;

        let set = prost_types::FileDescriptorSet::decode(rmail_proto::FILE_DESCRIPTOR_SET)
            .expect("the compiled descriptor set must decode");

        let mut out = Vec::new();
        for file in &set.file {
            let package = file.package();
            for service in &file.service {
                let fq = if package.is_empty() {
                    service.name().to_string()
                } else {
                    format!("{package}.{}", service.name())
                };
                for method in &service.method {
                    out.push((fq.clone(), method.name().to_string()));
                }
            }
        }
        assert!(!out.is_empty(), "descriptor set contained no services");
        out
    }

    #[test]
    fn health_and_reflection_are_public() {
        assert_eq!(
            lookup("/grpc.health.v1.Health/Check"),
            Some(&Requirement::Public)
        );
        assert_eq!(
            lookup("/grpc.reflection.v1.ServerReflection/ServerReflectionInfo"),
            Some(&Requirement::Public)
        );
    }

    #[test]
    fn mail_read_only_scope_is_denied_send_and_delete() {
        // The acceptance criterion, at the table level: whatever a read-only
        // token is granted, it is never `mail.write`/`mail.send`, and every
        // mutating row above requires one of those (or stronger).
        let read_only = Scope::MailRead;
        for method in [
            "/rmail.v1.MailService/Delete",
            "/rmail.v1.MailService/Move",
            "/rmail.v1.MailService/Copy",
            "/rmail.v1.MailService/SetFlags",
            "/rmail.v1.OutboxService/Send",
        ] {
            let Some(Requirement::Scope(required)) = lookup(method) else {
                unreachable!("{method} should require a scope");
            };
            assert!(
                !rmail_core::auth::satisfies(std::slice::from_ref(&read_only), required),
                "{method} (requires {required:?}) must not be satisfied by mail.read alone"
            );
        }
    }
}
