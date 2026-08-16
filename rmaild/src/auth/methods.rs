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
//! Add one `(method, Requirement)` row per new RPC below. A row may be
//! written *ahead* of the service it governs, so a task can land its RPCs
//! into a table that already expects them — but a provisional row is a guess,
//! and when the real service lands it is a starting point to confirm against
//! the real proto rather than settled fact: rename/add/remove rather than
//! assuming it was right.
//!
//! Three rounds of that have now happened. The `MailService` rows were
//! provisional until task 39 landed `proto/rmail/v1/mail.proto` and turned
//! out to need no changes. The `AiService` rows were provisional until task
//! 50 and *did* need changing — the guessed `Summarize`/`AskMailbox` matched
//! neither the real RPC names nor, for `AskMailbox`, this service at all. The
//! provisional `OutboxService/Send` row was replaced wholesale by task 61's
//! `SendSchedulerService` section: the real service is named differently and has no
//! method called `Send`, so that row could never have matched anything.
use rmail_core::auth::Scope;

/// What a method needs from the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    /// No authentication or scope required (health, reflection).
    Public,
    /// The caller's granted scopes must satisfy this one
    /// (see [`rmail_core::auth::satisfies`]).
    Scope(Scope),
    /// The caller's granted scopes must satisfy **at least one** of these.
    ///
    /// `rmail_core::auth::satisfies` has no scope hierarchy — only `Admin`
    /// covers anything else — so an RPC that two different kinds of operator
    /// legitimately need cannot be expressed as a single scope. Reach for
    /// this only where widening is genuinely risk-reducing (the one use is
    /// `CancelScheduled`, which can only ever *prevent* mail); a disjunction
    /// on an RPC with real authority is a way to accidentally grant it twice.
    AnyOf(&'static [Scope]),
    /// The caller's granted scopes must satisfy **every** one of these.
    ///
    /// The mirror image of [`Requirement::AnyOf`], and the one this table
    /// needed as soon as an RPC exercised two genuinely different authorities
    /// at once. `RuleService/EvaluateRules` is the motivating case: firing a
    /// rule runs an operator-configured hook (`automation`, exactly what
    /// `HookService/TestHook` sits behind) *and* moves, flags, and drafts
    /// replies to mail (`mail.write`, exactly what `MailService/Move` sits
    /// behind). Either alone under-gates it — an `automation`-only token could
    /// otherwise archive an inbox, and a `mail.write`-only token could
    /// otherwise spawn a process — and picking the stronger of the two would
    /// silently grant the other.
    ///
    /// `Scope::Admin` still satisfies this, since it satisfies each element
    /// (see [`rmail_core::auth::satisfies`]). An empty slice would grant
    /// everything, which is why no row here uses one and
    /// `no_all_of_row_is_empty` asserts none ever does.
    AllOf(&'static [Scope]),
}

impl Requirement {
    /// Whether a caller holding `granted` satisfies this requirement.
    ///
    /// The one definition of "satisfied" in the daemon. `crate::auth`'s layer
    /// applies it to decide whether a request proceeds, and
    /// `crate::mcp::projection` applies it to decide which tools a caller is
    /// even shown — two places that must agree, since a tool advertised to a
    /// token that is then refused it is a worse experience than not listing
    /// it, and a tool *withheld* from a token that would have been allowed is
    /// a capability silently lost.
    ///
    /// [`Requirement::Public`] is `true` for any scope set including the
    /// empty one: a public method needs no authentication at all.
    #[must_use]
    pub fn satisfied_by(&self, granted: &[Scope]) -> bool {
        match self {
            Requirement::Public => true,
            Requirement::Scope(scope) => rmail_core::auth::satisfies(granted, scope),
            Requirement::AnyOf(scopes) => scopes
                .iter()
                .any(|scope| rmail_core::auth::satisfies(granted, scope)),
            // The emptiness guard is not belt-and-braces: an empty `all` is
            // vacuously true, so an empty `AllOf` row would grant the method
            // to every caller. `no_all_of_row_is_empty` keeps one from being
            // written; this keeps one from being *honoured* if it were.
            Requirement::AllOf(scopes) => {
                !scopes.is_empty()
                    && scopes
                        .iter()
                        .all(|scope| rmail_core::auth::satisfies(granted, scope))
            }
        }
    }

    /// This requirement in the words an operator would use to fix it
    /// ("scope mail.read and ai.invoke").
    ///
    /// Shared by the `PERMISSION_DENIED` message the auth layer returns and
    /// by the MCP tool description, so an agent reading why a tool exists and
    /// an operator reading why a call failed are told the same thing.
    #[must_use]
    pub fn describe(&self) -> String {
        fn join(scopes: &[Scope], conjunction: &str) -> String {
            scopes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(conjunction)
        }
        match self {
            Requirement::Public => "no scope".to_owned(),
            Requirement::Scope(scope) => format!("scope {scope}"),
            Requirement::AnyOf(scopes) => format!("scope {}", join(scopes, " or ")),
            Requirement::AllOf(scopes) => format!("scope {}", join(scopes, " and ")),
        }
    }
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
    // -- AccountService OAuth (task 79) -------------------------------------
    // `admin`, and not a softer scope, because these three *are* credential
    // management: `BeginOAuth`/`CompleteOAuth` mint a grant that is a
    // non-expiring bearer credential for the whole mailbox and file it in the
    // Keychain, and `RefreshToken` spends a use of that grant at the provider
    // (which on Microsoft rotates it, invalidating the stored one). A
    // `mail.read` token that could re-point an account's credential at a
    // client id the caller controls would be a privilege escalation out of
    // this daemon entirely.
    (
        "/rmail.v1.AccountService/BeginOAuth",
        Requirement::Scope(Scope::Admin),
    ),
    (
        "/rmail.v1.AccountService/CompleteOAuth",
        Requirement::Scope(Scope::Admin),
    ),
    (
        "/rmail.v1.AccountService/RefreshToken",
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
    // -- ExportService (task 82) ---------------------------------------------
    // An export is a bulk local read: raw RFC822, parsed metadata, flags, and
    // — under `with_ai` — the stored AI artifacts. `mail.read` is exactly the
    // authority that already covers reading a message, and an export is that
    // in volume, not in kind. Deliberately *not* `admin` (the bar
    // `AuditService` sits behind): the audit ledger is the record of what was
    // sent to a provider and exists to hold the operator to account, while an
    // export returns mail the caller could already fetch one `MailService/Get`
    // at a time. Deliberately *not* `ai.invoke` under `with_ai` either — that
    // flag attaches artifacts the AI passes already produced and cannot cause
    // a model call (see `rmail_core::export`'s module docs), so gating it
    // behind spend authority would misdescribe what it does.
    (
        "/rmail.v1.ExportService/Export",
        Requirement::Scope(Scope::MailRead),
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
    // -- ComposeService (task 60) ---------------------------------------------
    // Drafts are local mail the caller authored, scoped the same read/write
    // way `NoteService`'s rows are: `GetDraft`/`ListDrafts` only read the
    // local database, so `mail.read` suffices; `CreateDraft`/`UpdateDraft`/
    // `DeleteDraft` mutate it, so they need `mail.write`. Nothing here
    // reaches IMAP, SMTP, or a model provider — this task builds the draft
    // and MIME layer only.
    // ConfigService (task 84) serves the client-side keymap. Asymmetric on
    // purpose. `GetKeymap` is `automation`: a command palette or an MCP tool
    // listing what a chord does is tooling, and gating it behind `admin` would
    // make the shared action-id registry the acceptance asks for unreachable
    // by exactly the clients it exists for. `SetBinding` is `admin`, because
    // it rewrites a file that changes what every keystroke on this machine
    // does — including, if it were allowed to, the keys a user quits with.
    //
    // Neither is `mail.read`/`mail.write`: no mail is read or written here.
    (
        "/rmail.v1.ConfigService/GetKeymap",
        Requirement::Scope(Scope::Automation),
    ),
    (
        "/rmail.v1.ConfigService/SetBinding",
        Requirement::Scope(Scope::Admin),
    ),
    (
        "/rmail.v1.ComposeService/CreateDraft",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.ComposeService/GetDraft",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.ComposeService/ListDrafts",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.ComposeService/UpdateDraft",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.ComposeService/DeleteDraft",
        Requirement::Scope(Scope::MailWrite),
    ),
    // `RenderDraft` is the odd one out: it sends nothing and persists
    // nothing, so on the "does it mutate?" test alone it would be
    // `mail.read`. It requires `mail.send` because of *what it produces* —
    // the exact, complete octets of a transmissible message, Message-ID and
    // all (`rmail_core::compose::mime`'s own module docs: the submission path
    // hands these bytes to SMTP unchanged). The property prd.md's scope model
    // promises is "you can hand Claude a token that reads and summarizes
    // freely but cannot send"; a read-only token that can mint a ready-to-
    // transmit message and hand it to any other SMTP client has been given
    // most of what that sentence says it cannot have.
    //
    // Two consequences of `Requirement` holding exactly one scope, with no
    // conjunction and no hierarchy below `Admin` (see
    // `rmail_core::auth::satisfies`): a `mail.read` + `mail.write` token —
    // the natural composer token — cannot preview its own draft, and
    // `mail.send` alone is now enough to *read* a draft's full body, since
    // the rendered message contains it. Neither is wrong for this surface,
    // but if `Requirement` ever grows a conjunction this row wants
    // `MailRead + MailSend` rather than either alone.
    (
        "/rmail.v1.ComposeService/RenderDraft",
        Requirement::Scope(Scope::MailSend),
    ),
    // -- SearchService (tasks 33, 37, 51) -------------------------------------
    // Every RPC here is a read-only query over the local index (no IMAP round
    // trip, no mutation — see `search_service`'s own module docs), so
    // `mail.read` is the right ceiling for all four: `Search`/`Semantic`
    // rank and stream messages the caller could already `MailService::List`,
    // and `Explain` only re-derives a rationale for one, already-visible
    // message. None of them needs `mail.write`.
    //
    // Task 51's L2 rerank means `Search` *can* reach a provider — but only
    // where the daemon's own `search.rerank` already selects Claude, because
    // `SearchApi::rerank_for` lets `SearchRequest.rerank` reduce the
    // configured backend and never escalate past it. That is what keeps this
    // row at `mail.read`: a read-scoped token can still only ask for the
    // reranking the operator already turned on, so it cannot spend or egress
    // anything the configuration did not already sanction. If that clamp is
    // ever loosened, `Search` needs `ai.invoke` the way
    // `AnalyzeMessage`/`SuggestReply` do.
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
    // -- AnalyticsService (task 71) ------------------------------------------
    // A response-time report is arithmetic over headers the caller could
    // already list one by one through `MailService/List`, so it needs exactly
    // what listing needs. It reaches no provider and writes nothing, which is
    // what keeps it off `ai.invoke` and off `mail.write`.
    (
        "/rmail.v1.AnalyticsService/GetResponseTimes",
        Requirement::Scope(Scope::MailRead),
    ),
    // `GenerateDigest` (task 70) is the opposite of its neighbour on every
    // axis the row above turns on, so it needs both scopes — the same pairing,
    // for the same two reasons, `AiService/AskMailbox` and
    // `AttachmentService/AskAttachment` carry.
    //
    // `mail.read`, because a briefing is built from the bodies of a whole
    // window of mail and restates them; anything that can read that much of a
    // mailbox has to need what reading a mailbox needs.
    //
    // `ai.invoke`, because calling the provider is the entire RPC. There is no
    // clamp-to-configured-backend argument available here of the kind that
    // keeps `SearchService/Search` at `mail.read`: a caller names the window,
    // and a wide enough window is an arbitrarily large Sonnet call charged to
    // the operator. `AllOf`, not `AnyOf` — either scope alone under-gates it,
    // and picking the stronger of the two would silently grant the other.
    (
        "/rmail.v1.AnalyticsService/GenerateDigest",
        Requirement::AllOf(&[Scope::MailRead, Scope::AiInvoke]),
    ),
    // -- IndexService (task 24) -----------------------------------------------
    // The index is a derived artifact over mail the caller can already read, so
    // the read-only RPCs sit at `mail.read` for the same reason `SearchService`'s
    // do: `Status` reports coverage and queue depth, `Verify` reports drift
    // without repairing any of it, and `ListEntities` enumerates things
    // extracted from messages a `mail.read` token could already fetch. None of
    // the three writes anything.
    (
        "/rmail.v1.IndexService/Status",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.IndexService/Verify",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.IndexService/ListEntities",
        Requirement::Scope(Scope::MailRead),
    ),
    // `Reindex` schedules and runs indexing work. It only ever *recomputes*
    // what the message store already implies — nothing it does can produce a
    // fact the mail did not already contain, and nothing it does is visible on
    // IMAP — so `mail.write` is the right level: the same "mutates local state"
    // test `SyncService/SyncFolder` sits behind, not the daemon-wide control
    // plane below.
    (
        "/rmail.v1.IndexService/Reindex",
        Requirement::Scope(Scope::MailWrite),
    ),
    // `Rebuild` and `Gc` are `admin`, deliberately a step above `Reindex`, and
    // this is why the two are separate RPCs at all (see `index_service`'s own
    // module docs): this table is keyed by method path and cannot look at a
    // request's fields, so a destructive mode *inside* `Reindex` would either
    // force every routine `mail index run` up to `admin` or leave a full index
    // wipe reachable with `mail.write`.
    //
    // `Rebuild` deletes the derived data for whole stages daemon-wide and
    // leaves search degraded until it is recomputed — hours, for a large
    // mailbox with embeddings on. `Gc` deletes rows outright. Both are the
    // "mutates shared, global state, un-scoped by account" class that
    // `AiService/SetPaused` and `AiService/RetryFailed` sit behind for exactly
    // the same reason, and neither is something a token minted to keep one
    // caller's mail indexed should be able to do to every other caller.
    (
        "/rmail.v1.IndexService/Rebuild",
        Requirement::Scope(Scope::Admin),
    ),
    (
        "/rmail.v1.IndexService/Gc",
        Requirement::Scope(Scope::Admin),
    ),
    // `SetPaused` is the daemon-wide indexing switch — it names no account and
    // no message, and stopping it stops new mail becoming searchable for every
    // caller this daemon serves. `AiService/SetPaused`'s row gives the argument
    // verbatim.
    (
        "/rmail.v1.IndexService/SetPaused",
        Requirement::Scope(Scope::Admin),
    ),
    // -- FinderService (task 59) ----------------------------------------------
    // `Find` reads a denormalized copy of subject lines, folder paths, contact
    // names, saved-search text and tag names — nothing a `mail.read` token
    // could not already fetch from `MailService`/`TagService`/`SearchService`,
    // so it sits at the same level `SearchService/Search` does. `IndexStatus`
    // reports counts and a timestamp about that same index and discloses less.
    (
        "/rmail.v1.FinderService/Find",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.FinderService/IndexStatus",
        Requirement::Scope(Scope::MailRead),
    ),
    // `BatchAction` archives, deletes, and re-flags mail — against several
    // messages at once, from a selection the caller made in a picker. It is
    // exactly the authority `MailService/Move`, `/Delete` and `/SetFlags` sit
    // behind, executed through the same `MailStore`, so it gets the same
    // scope. Not `AnyOf`, and not `admin`: widening an RPC that can empty an
    // inbox is the case that row's own doc comment warns against.
    (
        "/rmail.v1.FinderService/BatchAction",
        Requirement::Scope(Scope::MailWrite),
    ),
    // `RebuildIndex` re-derives the finder index from the source tables. It
    // recomputes only what the mail already implies, touches no other
    // subsystem, and leaves nothing degraded for other callers — the same
    // "mutates local derived state" test `IndexService/Reindex` sits behind,
    // not the daemon-wide `IndexService/Rebuild` one (which deletes whole
    // stages and leaves search broken for hours).
    (
        "/rmail.v1.FinderService/RebuildIndex",
        Requirement::Scope(Scope::MailWrite),
    ),
    // `Evaluate` (task 37) runs caller-supplied queries through the same
    // pipeline and reports aggregate metrics. It reads no more than `Search`
    // does — but note it *does* let a caller confirm whether a given
    // `Message-ID` exists in the corpus, by watching whether a judgment
    // resolves. That is strictly less than `Search` already discloses about
    // the same message, so it needs no scope of its own; it is called out
    // here so the inference is a considered decision rather than something
    // to rediscover later.
    (
        "/rmail.v1.SearchService/Evaluate",
        Requirement::Scope(Scope::MailRead),
    ),
    // `LogFeedback` (task 64) is the one `SearchService` RPC that writes, and
    // it is still `mail.read` rather than `mail.write`.
    //
    // The reason is what "write" means in this table everywhere else: every
    // `mail.write` row above mutates *mail* — a flag, a mailbox, a tag —
    // and most of them reflect that mutation to the IMAP server. This one
    // appends to a local, opt-outable telemetry log
    // (`rmail_core::feedback`); it cannot change a message, cannot be seen
    // by any other client, and cannot leave the machine. Requiring
    // `mail.write` would mean a read-only token — the exact token prd.md
    // describes handing to Claude to "read and summarize freely but not
    // send" — could search but never contribute the click data that makes
    // search better, which inverts the risk: the capability being granted
    // here is "make my own future searches more relevant".
    //
    // What a caller can do with it is bounded to that, and the bound is
    // enforced rather than assumed: the request names a `query_id` this
    // daemon minted for a page it already served, and
    // `feedback::repo::insert_actions` rejects — inside the same transaction
    // as the write — any action naming a message that query did not show.
    // Without that check a read-scoped token could attach arbitrary training
    // labels to arbitrary message ids under one of its own real `query_id`s,
    // and this row's reasoning would not hold. Nothing that survives the
    // check reveals or alters anything a `mail.read` token could not already
    // reach via `Search` itself.
    (
        "/rmail.v1.SearchService/LogFeedback",
        Requirement::Scope(Scope::MailRead),
    ),
    // `SearchAttachments` (task 74) discloses text extracted from attachments
    // of messages a `mail.read` token can already fetch whole via
    // `MailService/GetAttachment` — strictly less than that RPC hands over,
    // since this one returns a bounded excerpt rather than the bytes.
    //
    // It needs no `ai.invoke` for exactly the reason `SearchService/Search`
    // above does not, and the argument has to be the clamp one rather than a
    // stronger-sounding claim about egress. Its dense arm embeds the query
    // with `index.semantic`'s configured embedder, which under
    // `provider = "voyage"` is a metered third-party API — so "nothing here
    // leaves the host" would be false. What *is* true is that the embedder is
    // whatever the operator already indexes the mailbox with: a read-scoped
    // token can spend nothing the configuration had not already sanctioned,
    // and cannot select a backend. That is the same bound `Search`'s row
    // rests on. If this surface ever grows a reranker a request can *choose*,
    // it needs `ai.invoke` the way `AskAttachment` below has it.
    (
        "/rmail.v1.SearchService/SearchAttachments",
        Requirement::Scope(Scope::MailRead),
    ),
    // -- AttachmentService (task 74) ------------------------------------------
    // `AskAttachment` needs both scopes for the reasons `AiService/AskMailbox`
    // gives at length below, and the second half is if anything stronger here.
    //
    // `ai.invoke`, because calling the provider is the entire RPC: unlike
    // `Search`, whose row stays at `mail.read` only because
    // `SearchApi::rerank_for` clamps a request to the backend the operator
    // already sanctioned, an answer with no model call is not a degraded
    // answer but no answer. A `mail.read` token that could force provider
    // spend is the hole task 51's own review caught.
    //
    // `mail.read`, because `ai.invoke` alone would be a content escalation.
    // `AnalyzeMessage`/`SuggestReply` sit at `ai.invoke` and disclose one
    // message the caller already named. In its searched form this RPC takes a
    // free-text question, ranks attachments across every configured account,
    // and streams back verbatim quotes from documents — contracts, invoices,
    // scans — plus each source's mailbox, filename and UID. That is a
    // mailbox-wide read of exactly the material a mailbox's owner is least
    // willing to have gone fishing through.
    (
        "/rmail.v1.AttachmentService/AskAttachment",
        Requirement::AllOf(&[Scope::MailRead, Scope::AiInvoke]),
    ),
    // -- SendSchedulerService (task 61) ----------------------------------------------
    // Replaces the provisional `OutboxService/Send` row this table carried
    // until task 61 landed the real `proto/rmail/v1/send_scheduler.proto` —
    // per this table's own "confirm against the real proto... rename/add/
    // remove" note above. The real service is named `SendScheduler` (prd.md
    // III-5's proto sketch) and has no method called `Send`, so the old row
    // could never have matched anything; the acceptance case it existed for
    // (a `mail.read`-only token is physically denied a send) now runs against
    // `ScheduleSend` below.
    //
    // Anything that can put octets on the wire — now or later — is
    // `mail.send`. `ScheduleSend` is the obvious one; `SendNow` and
    // `RescheduleSend` are the same capability with a delay attached, and
    // `RetryFailed` re-arms a message the server already refused once.
    // `UpdateScheduledBody` changes *what* will be transmitted, which is the
    // send capability in the only sense that matters to a recipient.
    (
        "/rmail.v1.SendSchedulerService/ScheduleSend",
        Requirement::Scope(Scope::MailSend),
    ),
    (
        "/rmail.v1.SendSchedulerService/RescheduleSend",
        Requirement::Scope(Scope::MailSend),
    ),
    (
        "/rmail.v1.SendSchedulerService/UpdateScheduledBody",
        Requirement::Scope(Scope::MailSend),
    ),
    (
        "/rmail.v1.SendSchedulerService/SendNow",
        Requirement::Scope(Scope::MailSend),
    ),
    (
        "/rmail.v1.SendSchedulerService/RetryFailed",
        Requirement::Scope(Scope::MailSend),
    ),
    // `CancelScheduled` is the deliberate exception, and it takes *either*
    // scope. It is the only RPC here that can *stop* a transmission, and it
    // is the mechanism prd.md gives a human for intercepting an AI-originated
    // send ("always subject to the undo window so a human can intercept").
    //
    // `mail.write` alone was wrong in the other direction: with no scope
    // hierarchy, a `mail.send` token could schedule a send with a mandatory
    // undo window and then be unable to use it — the safety property exists
    // precisely for AI-originated sends, which are exactly the ones holding a
    // send-scoped token. `mail.send` alone would have the symmetric problem,
    // leaving a deliberately send-less operator unable to intervene.
    //
    // Widening here is strictly risk-reducing: the worst a caller can do with
    // cancel is prevent mail.
    (
        "/rmail.v1.SendSchedulerService/CancelScheduled",
        Requirement::AnyOf(&[Scope::MailWrite, Scope::MailSend]),
    ),
    // Reads over the local outbox: subjects, recipients, and state, which is
    // the same class of thing `MailService::List` returns for inbound mail.
    // `SuggestSendTime` is a read too — it proposes an instant and persists
    // nothing (prd.md: "no side effects — propose then `schedule_send`").
    (
        "/rmail.v1.SendSchedulerService/ListOutbox",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SendSchedulerService/WatchOutbox",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SendSchedulerService/SuggestSendTime",
        Requirement::Scope(Scope::MailRead),
    ),
    // Follow-ups are local reminders. Nothing about them reaches IMAP, SMTP,
    // or a model provider, so they take the same read/write split
    // `NoteService`'s rows do rather than borrowing `mail.send` from the
    // outbox they happen to share a service with.
    (
        "/rmail.v1.SendSchedulerService/CreateFollowup",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SendSchedulerService/DismissFollowup",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SendSchedulerService/ListFollowups",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SendSchedulerService/ListWaitingOn",
        Requirement::Scope(Scope::MailRead),
    ),
    // The pre-send guardian and the tracker's judge (task 63) both read a
    // message *and* call a model, so both scopes, the same `AllOf` shape
    // `AttachmentService/AskAttachment` uses. `mail.read` alone would let a
    // token that is deliberately kept away from the model provider spend
    // against `ai.limits`; `ai.invoke` alone would let a token with no
    // mailbox access have a draft's contents summarized back to it.
    //
    // Deliberately *not* `mail.send`, even though `PreflightCheck` sits on
    // the send path: it reviews and reports, and cannot put a byte on the
    // wire. Requiring the send scope to ask "is this message safe to send"
    // would mean the only tokens that could check a message are the ones that
    // could already send it unchecked.
    (
        "/rmail.v1.SendSchedulerService/PreflightCheck",
        Requirement::AllOf(&[Scope::MailRead, Scope::AiInvoke]),
    ),
    (
        "/rmail.v1.SendSchedulerService/DraftNudge",
        Requirement::AllOf(&[Scope::MailRead, Scope::AiInvoke]),
    ),
    // `TrackFollowup` writes a reminder as well as calling a model, so it
    // takes the write scope rather than the read one — the same split the
    // three `Followup` rows above make.
    (
        "/rmail.v1.SendSchedulerService/TrackFollowup",
        Requirement::AllOf(&[Scope::MailWrite, Scope::AiInvoke]),
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
    // `AskMailbox` (task 52) is the one row in this table that needs *both*
    // `mail.read` and `ai.invoke`, and neither half is redundant.
    //
    // `ai.invoke`, because unlike `Search` this RPC does not merely *reach*
    // a provider under some configurations — calling the provider is the
    // entire RPC. `SearchService/Search`'s row above stays at `mail.read`
    // only because `SearchApi::rerank_for` clamps a request to the backend
    // `search.rerank` already sanctioned, so a read-scoped token can spend
    // nothing the operator had not already turned on. There is no equivalent
    // clamp here and there could not be: an answer with no model call is not
    // a degraded answer, it is no answer. A `mail.read` token that could
    // force provider spend is the exact hole task 51's own review caught, and
    // this is where it would otherwise reopen.
    //
    // `mail.read`, because `ai.invoke` alone would be a *content* escalation.
    // `AnalyzeMessage`/`SuggestReply` sit at `ai.invoke` and disclose one
    // message the caller already named and could already fetch. This RPC
    // takes a free-text question, searches every configured account, and
    // streams back verbatim quotes plus each source's mailbox, sender,
    // subject and UID — a mailbox-wide read by any reasonable definition. A
    // token minted to summarize a message it was handed must not become a
    // way to go fishing through mail it was never given.
    (
        "/rmail.v1.AiService/AskMailbox",
        Requirement::AllOf(&[Scope::MailRead, Scope::AiInvoke]),
    ),
    // -- AiPolicyService (task 76) --------------------------------------------
    // Both rows are `admin`, and neither is `AiSpend`.
    //
    // `Scope::AiSpend(cap)` is tempting here — it is literally named for
    // dollars — but it is the wrong requirement, for the same structural
    // reason its own doc comment gives: this table is keyed by method name
    // alone and cannot see a request's amount, so requiring `AiSpend(n)`
    // would have to pick some fixed `n` unrelated to the budget being set.
    // Worse, the relationship runs backwards. `AiSpend` bounds what a token
    // may *spend*; `SetBudget` changes what *every* token may spend, for
    // every account this daemon serves. A token granted `ai.spend:5` would,
    // under an `AiSpend`-scoped SetBudget, be able to raise the global cap to
    // $500 and then spend it — the cap it was minted with would bound
    // nothing at all. Raising a spend limit is administration, not spending,
    // and sits behind `admin` exactly as `AiService/SetPaused` does for the
    // same "mutates shared, global state" reason.
    (
        "/rmail.v1.AiPolicyService/SetBudget",
        Requirement::Scope(Scope::Admin),
    ),
    // GetSpend is `admin` for the reason `AiService/GetUsage`'s row above
    // gives verbatim: it exposes *aggregate spend*, not an answer about one
    // message the caller already named. A token minted to summarize mail
    // should not also be able to read the account's total AI dollar spend.
    (
        "/rmail.v1.AiPolicyService/GetSpend",
        Requirement::Scope(Scope::Admin),
    ),
    // -- AiSafetyService (task 77) --------------------------------------------
    // The two RPCs of the prompt-injection shield sit at very different
    // privileges, and the gap between them is the point of splitting them.
    //
    // `ScanInjection` reads. It makes no provider call at all — the detector
    // is a local pattern scan over text this daemon already holds — so
    // `ai.invoke` would be the wrong requirement twice over: it neither
    // spends nor invokes anything, and requiring it would mean a token that
    // may read mail but not call a model could not see that a message tried
    // to attack one. What it *does* return is quoted message content (the
    // excerpts are the whole point of the answer), which is exactly what
    // `mail.read` governs everywhere else in this table.
    (
        "/rmail.v1.AiSafetyService/ScanInjection",
        Requirement::Scope(Scope::MailRead),
    ),
    // `ConfirmInjection` is the release valve on a *fail-closed* control, so
    // it inherits the authority of what it releases rather than of what it
    // touches. Its entire effect is that a rule whose `claude_is` matched a
    // flagged message may now move, archive, label, run a hook and draft a
    // reply — the same pair `RuleService/EvaluateRules` requires, and for the
    // identical reason its own row gives: `automation` alone would let a
    // token that cannot write mail cause mail to be written, and `mail.write`
    // alone would let a token that cannot run automation cause a process to
    // be spawned. `ai.invoke` is deliberately *not* in the set: confirming
    // spends nothing, and a caller who has already been trusted with both
    // halves of "a rule may act" should not additionally need the scope for
    // paying a provider.
    (
        "/rmail.v1.AiSafetyService/ConfirmInjection",
        Requirement::AllOf(&[Scope::Automation, Scope::MailWrite]),
    ),
    // -- HookService (task 67) ------------------------------------------------
    // Hooks execute operator-configured shell commands (config-driven, never
    // user-supplied at the RPC layer — see `rmail_core::hooks`'s own module
    // docs) — `automation` is exactly the scope this table's own header
    // anticipated for the rules/hooks/webhooks surface (`Scope::Automation`'s
    // doc comment: "Rules/hooks/webhooks automation surfaces"). ListHooks only
    // reads the configured (already locally-visible-in-the-config-file) hook
    // list; TestHook actually spawns one. Both sit behind the same scope
    // rather than splitting read/write the way `MailService` does, because a
    // hook's command is itself already an admin-authored artifact — nothing
    // about *reading* the list is more sensitive than *running* an entry from
    // it, unlike mail content where reading and mutating are genuinely
    // different privileges.
    (
        "/rmail.v1.HookService/ListHooks",
        Requirement::Scope(Scope::Automation),
    ),
    (
        "/rmail.v1.HookService/TestHook",
        Requirement::Scope(Scope::Automation),
    ),
    // -- NotificationService (task 81) ----------------------------------------
    // `ScoreMessage` can enqueue a provider call for a message that has not
    // been scored yet, which is spend — so it sits behind `ai.invoke`, exactly
    // where every other RPC that can reach a model sits, rather than behind
    // `mail.read` on the grounds that it mostly reads a table. The read-only
    // half of its answer is not separable: a caller that only wanted to look
    // would be indistinguishable at this layer from one that wanted the call
    // made.
    //
    // `StreamAlerts` is `mail.read`. What an alert carries is a subject line,
    // a sender and a one-line summary of a message — mail content by any
    // honest reading, and the same privilege `MailService`'s reads and
    // `SyncService/WatchEvents` sit behind. It is deliberately *not*
    // `automation` despite being a notification surface: nothing about
    // reading this stream runs anything.
    (
        "/rmail.v1.NotificationService/ScoreMessage",
        Requirement::Scope(Scope::AiInvoke),
    ),
    (
        "/rmail.v1.NotificationService/StreamAlerts",
        Requirement::Scope(Scope::MailRead),
    ),
    // -- TagService (task 55) -------------------------------------------------
    // `ListTags`/`SuggestTags` are pure reads over the local mirror (no IMAP
    // round trip — `SuggestTags` in particular never calls a model, see
    // `rmaild::tag_service`'s own docs), so `mail.read` is the right ceiling,
    // the same reasoning `MailService`'s reads sit behind. Every mutation
    // (`AddTag`/`RemoveTag`/`CreateTag`/`BulkTag`/`ResolveSuggestion`) can
    // reflect to IMAP and always writes `message_tags`/`tags`, so those sit
    // behind `mail.write` — matching `MailService::SetFlags`'s row exactly,
    // since a tag is conceptually the same kind of per-message annotation a
    // flag is.
    (
        "/rmail.v1.TagService/ListTags",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.TagService/SuggestTags",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.TagService/AddTag",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.TagService/RemoveTag",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.TagService/CreateTag",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.TagService/BulkTag",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.TagService/ResolveSuggestion",
        Requirement::Scope(Scope::MailWrite),
    ),
    // -- SavedSearchService (task 35) -----------------------------------------
    // Reads sit at `mail.read` for the same reason `SearchService`'s do: they
    // rank or enumerate messages the caller could already `MailService::List`,
    // over the local index, with no IMAP round trip.
    // `ListSmartFolderMembers` is a *read* despite naming a folder — it runs
    // the predicate and streams what currently matches; it evaluates nothing
    // and fires no action (see `rmaild::saved_search_service`'s module docs).
    //
    // Everything that persists a definition (`Create*`/`Update*`/`Delete*`)
    // needs `mail.write`, and so does `EvaluateSmartFolder` — it is the one
    // RPC here with side effects, since a genuinely new member can auto-tag
    // (a `message_tags` write that may reflect to IMAP, exactly like
    // `TagService::AddTag`) and notify.
    (
        "/rmail.v1.SavedSearchService/CreateSavedSearch",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SavedSearchService/UpdateSavedSearch",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SavedSearchService/ListSavedSearches",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SavedSearchService/DeleteSavedSearch",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SavedSearchService/RunSavedSearch",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SavedSearchService/CreateSmartFolder",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SavedSearchService/ListSmartFolders",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SavedSearchService/DeleteSmartFolder",
        Requirement::Scope(Scope::MailWrite),
    ),
    (
        "/rmail.v1.SavedSearchService/ListSmartFolderMembers",
        Requirement::Scope(Scope::MailRead),
    ),
    (
        "/rmail.v1.SavedSearchService/EvaluateSmartFolder",
        Requirement::Scope(Scope::MailWrite),
    ),
    // -- RuleService (task 66) ------------------------------------------------
    // These rows are the reason `Requirement::AllOf` exists; see its own doc
    // comment. A rule is not one privilege — it is an unattended program that
    // moves mail, spawns a configured hook, writes a reply draft, and spends
    // money at a model provider — and the scopes below take that apart rather
    // than collapsing it into whichever single scope happens to be strongest.
    //
    // `ListRules` is the deliberate low-water mark: reading the automation
    // config is `automation` alone, exactly as `HookService/ListHooks` is.
    // Everything that can *act* needs strictly more, which is the distinction
    // "a rule that can draft a reply is not the same privilege as listing
    // rules" names.
    (
        "/rmail.v1.RuleService/ListRules",
        Requirement::Scope(Scope::Automation),
    ),
    // `CreateRule` persists a rule the background evaluator will then fire,
    // unattended, against every new message — with no token involved at fire
    // time. Creating one is therefore *granting* authority to the daemon, not
    // merely writing config, and the grant is durable: `mail.write` because
    // the rule moves, flags, and drafts replies to mail forever after, and
    // `ai.invoke` because a `claude_is` predicate spends at the provider on
    // every new message forever after. That is strictly more than the one-shot
    // spend `SynthesizeRule`/`BacktestRule` need `ai.invoke` for below, so it
    // cannot need less.
    (
        "/rmail.v1.RuleService/CreateRule",
        Requirement::AllOf(&[Scope::Automation, Scope::MailWrite, Scope::AiInvoke]),
    ),
    // The only RPC here that fires actions: move/archive (`MailService/Move`
    // is `mail.write`), add_labels/add_flags (`TagService/AddTag` and
    // `MailService/SetFlags`, same), draft_reply (`ComposeService/CreateDraft`,
    // same), and run_hook (`HookService/TestHook` is `automation`). Note it is
    // *not* `mail.send`: a draft is not a transmission, and this service has no
    // path to SMTP — `ComposeService/RenderDraft` draws that line and it is
    // drawn the same way here.
    //
    // `ai.invoke` is on this row for exactly the reason it is on
    // `BacktestRule`'s below: one call can classify hundreds of messages, and
    // this table cannot see whether the rules in the request carry a
    // `claude_is` at all. An RPC that may spend at a provider requires the
    // scope named for spending at a provider, with no exception for the one
    // that also mutates mail.
    (
        "/rmail.v1.RuleService/EvaluateRules",
        Requirement::AllOf(&[Scope::Automation, Scope::MailWrite, Scope::AiInvoke]),
    ),
    // `SynthesizeRule` and `BacktestRule` mutate nothing — the engine's
    // dry-run path never claims and never calls the action runner — so they do
    // not need `mail.write`. They *do* both call the provider: synthesis
    // always, and a backtest for every `claude_is` decision the cache does not
    // already hold. `ai.invoke` is exactly the scope `AiService/AnalyzeMessage`
    // sits behind for the same reason, and it is required even for a purely
    // deterministic rule because this table cannot see whether the rule in the
    // request has a `claude_is` at all.
    //
    // Both also disclose mail content (a backtest reports subjects and the
    // model's explanation of a message), which `ai.invoke` already implies
    // elsewhere: `AnalyzeMessage` returns a summary of a message with that
    // scope alone.
    (
        "/rmail.v1.RuleService/SynthesizeRule",
        Requirement::AllOf(&[Scope::Automation, Scope::AiInvoke]),
    ),
    (
        "/rmail.v1.RuleService/BacktestRule",
        Requirement::AllOf(&[Scope::Automation, Scope::AiInvoke]),
    ),
    // `RecordCorrection` calls no provider and mutates no mail, so it sits
    // below everything that can act. It does, however, *copy message content*
    // — it freezes the rendered subject and body of the corrected message into
    // `rule_examples` — so it takes `mail.read` alongside `automation` rather
    // than `automation` alone. A token that cannot read mail should not be
    // able to cause mail to be read and stored somewhere new.
    (
        "/rmail.v1.RuleService/RecordCorrection",
        Requirement::AllOf(&[Scope::Automation, Scope::MailRead]),
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

    /// Every capability in the feature-parity registry has a scope row.
    ///
    /// The two tables are keyed by the same string and maintained for
    /// different reasons — this one says what a caller must hold,
    /// `rmail_core::parity` says what the capability *is* and what each
    /// surface calls it — so they can drift apart in either direction. The
    /// descriptor-set check above already covers "an RPC with no scope row",
    /// but it does not cover the case where a row is written here against a
    /// path the registry spells differently: both would look internally
    /// consistent while `Command::rpc` and `lookup` disagreed about the same
    /// method, which is exactly what task 53's projection joins on.
    #[test]
    fn every_parity_capability_has_a_scope_row() {
        for command in rmail_core::parity::Command::ALL {
            assert!(
                lookup(command.rpc()).is_some(),
                "{} ({}) is a declared capability with no row in this table, so the \
                 fail-closed default denies every call to it",
                command.name(),
                command.rpc()
            );
        }
    }

    /// The two tables' independent judgments about the same RPC agree.
    ///
    /// `rmail_core::parity::Effect` and this table's `Requirement` are decided
    /// separately: one is "does calling it change anything", the other is
    /// "what must the caller hold". They are not the same question — a write
    /// can legitimately sit at `mail.read` (`SearchService/LogFeedback`, whose
    /// row above argues the case at length) — but two combinations are always
    /// a mistake in one table or the other, and task 53 gates generated MCP
    /// tools on exactly this pair:
    ///
    /// - a capability that mutates and is `Public` would be an unauthenticated
    ///   write;
    /// - a capability marked `Read` whose row demands `mail.write`/`mail.send`
    ///   /`ai.invoke` means one of the two files has the wrong idea about what
    ///   it does, and if it is this one that is right, MCP would project a
    ///   mutation as a safe tool.
    ///
    /// `ai.invoke` is in that set for a reason found the hard way:
    /// `SynthesizeRule`/`BacktestRule` were first written here as `Read`
    /// because they act on no mail, while this table already required
    /// `ai.invoke` for them because they spend at the provider — which is one
    /// of the effects `parity::Effect` explicitly counts. Two files disagreed
    /// in exactly the way this test exists to notice, and without `ai.invoke`
    /// in the set it did not notice.
    #[test]
    fn effect_and_scope_agree_about_what_each_capability_does() {
        for command in rmail_core::parity::Command::ALL {
            let Some(requirement) = lookup(command.rpc()) else {
                // `every_parity_capability_has_a_scope_row` reports this
                // properly; skipping keeps the two failures from overlapping.
                continue;
            };
            let required: Vec<&Scope> = match requirement {
                Requirement::Public => Vec::new(),
                Requirement::Scope(scope) => vec![scope],
                Requirement::AnyOf(scopes) | Requirement::AllOf(scopes) => scopes.iter().collect(),
            };

            if command.effect().is_mutating() {
                assert!(
                    !required.is_empty(),
                    "{} mutates and requires no scope at all",
                    command.rpc()
                );
            } else {
                for scope in required {
                    assert!(
                        !matches!(scope, Scope::MailWrite | Scope::MailSend | Scope::AiInvoke),
                        "{} is declared read-only in rmail_core::parity but this table puts it \
                         behind {scope:?} — one of the two is wrong, and if this one is right \
                         then MCP would project a mutation as a safe tool",
                        command.rpc()
                    );
                }
            }
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

    /// The destructive index verbs sit above the read-only ones.
    ///
    /// Checked as a relation rather than as two literal rows: the property that
    /// matters is "whatever `Status` needs, `Rebuild`/`Gc` need strictly more,"
    /// and a future re-scoping that raised `Status` to `admin` would silently
    /// satisfy a pair of literal assertions while flattening the distinction
    /// this pair of rows exists to draw.
    #[test]
    fn the_destructive_index_verbs_need_more_than_reading_index_status() {
        let Some(Requirement::Scope(status)) = lookup("/rmail.v1.IndexService/Status") else {
            unreachable!("IndexService/Status should require a scope");
        };
        for method in [
            "/rmail.v1.IndexService/Rebuild",
            "/rmail.v1.IndexService/Gc",
            "/rmail.v1.IndexService/SetPaused",
        ] {
            let Some(Requirement::Scope(required)) = lookup(method) else {
                unreachable!("{method} should require a scope");
            };
            assert!(
                !rmail_core::auth::satisfies(std::slice::from_ref(status), required),
                "{method} (requires {required:?}) must need more than {status:?}, which only \
                 buys a read of the index's status"
            );
        }
    }

    #[test]
    fn a_send_scoped_token_can_use_the_undo_window_it_was_given() {
        // The mandatory undo window on an AI-originated send exists so a
        // human can intercept it. AI-originated sends are exactly the ones
        // made with a send-scoped token, and `satisfies` has no hierarchy --
        // so scoping cancel to `mail.write` alone left the caller holding an
        // undo window it could not use. Both scopes have to work.
        let Some(Requirement::AnyOf(required)) =
            lookup("/rmail.v1.SendSchedulerService/CancelScheduled")
        else {
            unreachable!("CancelScheduled should accept either scope");
        };
        for scope in [Scope::MailSend, Scope::MailWrite] {
            assert!(
                required
                    .iter()
                    .any(|want| rmail_core::auth::satisfies(std::slice::from_ref(&scope), want)),
                "{scope:?} must be able to cancel a scheduled send"
            );
        }
        // ...but it is still not open to a read-only token.
        let read_only = Scope::MailRead;
        assert!(
            !required
                .iter()
                .any(|want| rmail_core::auth::satisfies(std::slice::from_ref(&read_only), want)),
            "mail.read alone must not be able to cancel"
        );
    }

    /// An `AllOf` row with no scopes would be vacuously satisfied — every
    /// authenticated caller would pass. `authorize` guards against it too, but
    /// the row should never exist in the first place.
    #[test]
    fn no_all_of_row_is_empty() {
        for (method, requirement) in TABLE {
            if let Requirement::AllOf(scopes) = requirement {
                assert!(
                    !scopes.is_empty(),
                    "{method} has an empty AllOf, which grants it to everyone"
                );
            }
        }
    }

    /// Firing a rule is two authorities at once, and neither alone buys it.
    ///
    /// A rule runs an operator-configured hook (`automation`) *and* moves,
    /// flags, and drafts replies to mail (`mail.write`). Checked as a relation
    /// rather than as literal rows: the property that matters is that each
    /// scope on its own is insufficient, which a pair of `assert_eq!`s on the
    /// row's contents would keep asserting even if `authorize` stopped
    /// treating `AllOf` as a conjunction.
    #[test]
    fn evaluating_a_rule_needs_both_automation_and_mail_write() {
        for method in [
            "/rmail.v1.RuleService/EvaluateRules",
            "/rmail.v1.RuleService/CreateRule",
        ] {
            let Some(Requirement::AllOf(required)) = lookup(method) else {
                unreachable!("{method} should require every one of a scope set");
            };
            for granted in [Scope::Automation, Scope::MailWrite, Scope::MailRead] {
                assert!(
                    !required.iter().all(|want| rmail_core::auth::satisfies(
                        std::slice::from_ref(&granted),
                        want
                    )),
                    "{granted:?} alone must not be enough to fire {method}"
                );
            }
            // ...and holding the whole set is.
            let all: Vec<Scope> = required.to_vec();
            assert!(
                required
                    .iter()
                    .all(|want| rmail_core::auth::satisfies(&all, want)),
                "the full scope set must be enough for {method}"
            );
            // A rule that can spend at a provider requires the scope named for
            // it, whether or not it also mutates mail.
            assert!(
                required.contains(&Scope::AiInvoke),
                "{method} can spend at a model provider and must require ai.invoke"
            );
        }
    }

    /// Asking the mailbox a question is two authorities at once, and neither
    /// alone buys it.
    ///
    /// The failure this guards against is specific and has happened once
    /// already in this codebase's history: a `mail.read` token that can force
    /// provider spend. `AskMailbox` always calls the model — there is no
    /// clamp-to-configured-backend escape hatch the way `Search` has — so
    /// `mail.read` alone must not reach it. The mirror case matters just as
    /// much: `ai.invoke` alone would turn a token minted to summarize one
    /// named message into a mailbox-wide content search.
    ///
    /// Checked as a relation rather than as literal rows, for the reason
    /// `evaluating_a_rule_needs_both_automation_and_mail_write` gives: a pair
    /// of `assert_eq!`s on the row's contents would keep passing even if
    /// `authorize` stopped treating `AllOf` as a conjunction.
    #[test]
    fn asking_the_mailbox_needs_both_mail_read_and_ai_invoke() {
        let method = "/rmail.v1.AiService/AskMailbox";
        let Some(Requirement::AllOf(required)) = lookup(method) else {
            unreachable!("{method} should require every one of a scope set");
        };
        assert!(required.contains(&Scope::MailRead));
        assert!(required.contains(&Scope::AiInvoke));
        for granted in [Scope::MailRead, Scope::AiInvoke, Scope::MailWrite] {
            assert!(
                !required
                    .iter()
                    .all(|want| rmail_core::auth::satisfies(std::slice::from_ref(&granted), want)),
                "{granted:?} alone must not be enough to ask the mailbox a question"
            );
        }
        let both = [Scope::MailRead, Scope::AiInvoke];
        assert!(
            required
                .iter()
                .all(|want| rmail_core::auth::satisfies(&both, want)),
            "mail.read + ai.invoke must be enough to ask the mailbox a question"
        );
    }

    /// Asking an attachment a question is two authorities at once, and
    /// searching attachments is neither of them.
    ///
    /// The pair is checked together because the *gap* between the two rows is
    /// the design: `SearchAttachments` reads the local index and must stay
    /// reachable by a routine read-only token, while `AskAttachment` spends
    /// at a provider and must not be. Collapsing them — by raising the search
    /// or lowering the ask — would either make attachment search unusable for
    /// the tokens it exists for or hand every read-only token a way to spend
    /// money, and only one of those two mistakes is loud.
    #[test]
    fn asking_an_attachment_needs_both_mail_read_and_ai_invoke() {
        let Some(Requirement::Scope(searching)) =
            lookup("/rmail.v1.SearchService/SearchAttachments")
        else {
            unreachable!("SearchAttachments should require a single scope");
        };
        assert!(
            rmail_core::auth::satisfies(std::slice::from_ref(&Scope::MailRead), searching),
            "a read-only token must still be able to search attachments"
        );

        let method = "/rmail.v1.AttachmentService/AskAttachment";
        let Some(Requirement::AllOf(required)) = lookup(method) else {
            unreachable!("{method} should require every one of a scope set");
        };
        assert!(required.contains(&Scope::MailRead));
        assert!(required.contains(&Scope::AiInvoke));
        for granted in [Scope::MailRead, Scope::AiInvoke, Scope::MailWrite] {
            assert!(
                !required
                    .iter()
                    .all(|want| rmail_core::auth::satisfies(std::slice::from_ref(&granted), want)),
                "{granted:?} alone must not be enough to ask an attachment a question"
            );
        }
        // ...and whatever buys the search must not also buy the ask.
        assert!(
            !required
                .iter()
                .all(|want| rmail_core::auth::satisfies(std::slice::from_ref(searching), want)),
            "{searching:?} buys an attachment search and must not also buy provider spend"
        );
        let both = [Scope::MailRead, Scope::AiInvoke];
        assert!(
            required
                .iter()
                .all(|want| rmail_core::auth::satisfies(&both, want)),
            "mail.read + ai.invoke must be enough to ask an attachment a question"
        );
    }

    /// Listing rules is strictly less than firing one.
    ///
    /// This is the "a rule that can draft a reply is not the same privilege as
    /// listing rules" distinction, asserted as a relation: whatever `ListRules`
    /// needs must not be enough for `EvaluateRules`.
    #[test]
    fn listing_rules_is_not_enough_to_fire_one() {
        let Some(Requirement::Scope(listing)) = lookup("/rmail.v1.RuleService/ListRules") else {
            unreachable!("ListRules should require a single scope");
        };
        let Some(Requirement::AllOf(firing)) = lookup("/rmail.v1.RuleService/EvaluateRules") else {
            unreachable!("EvaluateRules should require every one of a scope set");
        };
        assert!(
            !firing
                .iter()
                .all(|want| rmail_core::auth::satisfies(std::slice::from_ref(listing), want)),
            "{listing:?} buys a listing and must not also buy an unattended mail mutation"
        );
    }

    /// The two model-calling rules RPCs require `ai.invoke`, the same scope
    /// every other provider-calling RPC in this table sits behind.
    #[test]
    fn the_model_calling_rule_rpcs_require_ai_invoke() {
        for method in [
            "/rmail.v1.RuleService/SynthesizeRule",
            "/rmail.v1.RuleService/BacktestRule",
        ] {
            let Some(Requirement::AllOf(required)) = lookup(method) else {
                unreachable!("{method} should require every one of a scope set");
            };
            assert!(
                required.contains(&Scope::AiInvoke),
                "{method} can spend at a model provider and must require ai.invoke"
            );
            assert!(
                !required.contains(&Scope::MailWrite),
                "{method} is a dry run and must not demand a mutation scope it never uses"
            );
        }
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
            "/rmail.v1.SendSchedulerService/ScheduleSend",
            "/rmail.v1.SendSchedulerService/SendNow",
            "/rmail.v1.SendSchedulerService/RetryFailed",
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
