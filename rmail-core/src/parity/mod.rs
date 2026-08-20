//! The feature-parity registry: one row per capability, reconciled against
//! every surface that exposes it (prd.md's "Design invariant: *If the CLI can
//! do it, gRPC can do it. If gRPC can't do it, it isn't a feature. If gRPC can
//! do it, Claude can do it (via MCP auto-projection).*"; task 41).
//!
//! # Why this is keyed by RPC
//!
//! The invariant above has gRPC on both sides of it: the CLI and the TUI must
//! not be able to do anything the API cannot, and MCP must be able to do
//! everything the API can. One row per *RPC* is therefore the only key that
//! can carry both halves — a row keyed by CLI verb could not say anything
//! about an RPC no CLI verb reaches, and there are many (`MailService/List`,
//! every `RuleService` and `SavedSearchService` method, `ComposeService`'s
//! whole draft surface). Those are not gaps to close in this file; they are
//! exactly the RPCs MCP will project that no human types.
//!
//! So [`Command`] has one variant per method in `proto/rmail/v1/*.proto`, and
//! the CLI paths and TUI action ids that reach it hang off that variant as
//! (possibly empty) lists. The empty list is the readable statement that a
//! capability has no human surface yet; the *reverse* — a human surface with
//! no capability — is what this module exists to make impossible.
//!
//! # The drift checks are tests, and they fail by name
//!
//! `tests.rs` reconciles this table against the **compiled** form of each
//! surface, never against a second hand-written list:
//!
//! | reconciled against | what a mismatch means |
//! |---|---|
//! | `rmail_proto::FILE_DESCRIPTOR_SET` | an RPC exists that no row claims (or a row names a method that does not exist) |
//! | [`crate::keymap::Action::ALL`] | a TUI action exists that is neither a capability nor declared UI-local |
//! | `clap`'s own command tree (in `rmail-cli`, which owns the `Cli` type) | a `mail` verb exists that no capability backs |
//!
//! A check that compared two hand-written lists would prove only that
//! somebody edited both. Each of these compares this table against something
//! *generated* from the surface itself, so a new RPC, a new action, or a new
//! subcommand fails the suite by name until a row is written for it — the
//! same shape as `rmaild::auth::methods`'
//! `every_rpc_in_the_descriptor_set_has_a_scope_row`, which is the check that
//! caught `AuditService` shipping deny-everything.
//!
//! # What these checks do *not* catch
//!
//! They reconcile the *existence* of a verb, an action or an RPC — not the
//! content of a claim. `every_cli_command_is_backed_by_a_capability` asks
//! whether some row claims `mail sync`; it cannot tell whether the row names
//! the RPCs that verb really calls. So adding a `--pause` flag to `mail sync`
//! that called `SyncService/Pause` would pass every test here, because
//! `mail sync` is already claimed. The same is true of the TUI: a new `Cmd`
//! issued from an already-declared action is invisible to
//! `every_tui_action_is_a_capability_or_declared_local`.
//!
//! Nothing generated exists to reconcile that against — the call sites are
//! ordinary Rust, not a table — so the honest statement is that the `cli:` and
//! `actions:` lists are maintained by hand and reviewed, while the *set* of
//! verbs, actions and RPCs is enforced. `the_verbs_that_reach_two_rpcs_still_do`
//! pins the rows where this has already bitten rather than pretending to be a
//! net.
//!
//! # Extending this table
//!
//! Add one variant per new RPC, in its service's block. The variant name is
//! mechanically the service name without its `Service` suffix, followed by
//! the method name (`MailService/Get` → `MailGet`), and
//! `every_variant_is_named_after_its_rpc` enforces that — a row whose name
//! and path disagree is a row someone will one day read as governing a
//! different method than it does.
//!
//! # The seam this leaves for task 53 (gRPC → MCP auto-projection)
//!
//! Task 53 generates MCP tools "at runtime from the compiled descriptor set +
//! per-RPC annotations (safe/mutating, tool name, arg mapping)". This table is
//! those annotations, and [`Command::for_rpc`] is the join: walk the
//! descriptor set, look each method up here, and you have the tool's name
//! ([`Command::tool`]), its description ([`Command::summary`]) and whether it
//! is safe or mutating ([`Command::effect`]). A method with no row cannot
//! happen — `every_rpc_in_the_descriptor_set_has_a_command` fails first — so
//! the projection has no "unknown RPC" case to invent a policy for.
//!
//! Two things are deliberately *not* here, because the descriptor set already
//! has them and a second copy could only drift: whether an RPC streams
//! (`MethodDescriptorProto::server_streaming`) and its argument shape (the
//! request message's own fields, which is task 53's "arg mapping"). The scope
//! an MCP tool is gated by is likewise not duplicated — that is
//! `rmaild::auth::methods::lookup`, keyed by the same path string
//! [`Command::rpc`] returns.

#[cfg(test)]
mod tests;

use crate::keymap::Action;

/// Whether calling a capability changes anything.
///
/// The "safe/mutating" annotation task 53 gates MCP tools with. The line is
/// drawn at *authority*, not at whether a row is written to a table: a
/// capability is [`Effect::Read`] only if a caller holding it could not, by
/// calling it, cause any effect an observer outside this process could see.
/// That is why `ComposeService/RenderDraft` is [`Effect::Mutate`] despite
/// persisting nothing — it emits the exact octets of a transmissible message,
/// which is why `rmaild::auth::methods` puts it behind `mail.send`.
///
/// Spend at a model provider is such an effect, which is what separates
/// `AiService/AskMailbox` ([`Effect::Mutate`]) from `SearchService/Search`
/// ([`Effect::Read`]) even though both can reach Claude: `SearchApi::rerank_for`
/// clamps a search to the backend `search.rerank` already sanctioned, so a
/// caller cannot spend anything the operator had not already turned on, while
/// an ask with no model call is not a degraded answer but no answer at all.
/// `rmaild::auth::methods` splits the same pair the same way and for the same
/// reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Effect {
    /// Observes state. Safe to project as a tool a read-only caller may use.
    Read,
    /// Changes state, or produces something carrying the authority to.
    Mutate,
}

impl Effect {
    /// Whether this capability changes anything.
    #[must_use]
    pub const fn is_mutating(self) -> bool {
        matches!(self, Self::Mutate)
    }
}

/// Declares the capability registry once, and derives the enum, the ordered
/// list, and every accessor from it.
///
/// One list rather than six parallel ones, for the reason
/// [`crate::keymap`]'s `actions!` macro gives: a capability added to the enum
/// but forgotten in `ALL` would be invisible to every drift check in
/// `tests.rs`, which is precisely the failure this file exists to prevent.
macro_rules! commands {
    ($(
        $variant:ident {
            rpc: $rpc:literal,
            tool: $tool:literal,
            effect: $effect:ident,
            cli: [ $($cli:literal),* $(,)? ],
            actions: [ $($action:ident),* $(,)? ],
            summary: $summary:literal $(,)?
        }
    )*) => {
        /// One capability of the rmail API, addressed by the RPC that is its
        /// definition.
        ///
        /// See the module docs: the CLI, the TUI and (from task 53) MCP are
        /// adapters over this list, and the tests reconcile each of them
        /// against it.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Command {
            $( #[doc = $summary] $variant, )*
        }

        impl Command {
            /// Every capability, in the order this table declares them
            /// (grouped by service).
            pub const ALL: &'static [Command] = &[ $( Command::$variant, )* ];

            /// This variant's own name, for failure messages that have to
            /// name the row a human must go and edit.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self { $( Command::$variant => stringify!($variant), )* }
            }

            /// The fully-qualified gRPC method path that *is* this
            /// capability, e.g. `/rmail.v1.MailService/Get` — the same string
            /// `rmaild::auth::methods::lookup` is keyed by.
            #[must_use]
            pub const fn rpc(self) -> &'static str {
                match self { $( Command::$variant => $rpc, )* }
            }

            /// The MCP tool name this capability projects to (task 53).
            #[must_use]
            pub const fn tool(self) -> &'static str {
                match self { $( Command::$variant => $tool, )* }
            }

            /// Whether calling it changes anything.
            #[must_use]
            pub const fn effect(self) -> Effect {
                match self { $( Command::$variant => Effect::$effect, )* }
            }

            /// The `mail` subcommand paths that reach this capability, space
            /// separated and without the `mail` itself (`"ai budget set"`).
            /// Empty when no CLI verb reaches it yet.
            #[must_use]
            pub const fn cli(self) -> &'static [&'static str] {
                match self { $( Command::$variant => &[ $($cli,)* ], )* }
            }

            /// The TUI actions that reach this capability. Empty when none
            /// does.
            #[must_use]
            pub const fn actions(self) -> &'static [Action] {
                match self { $( Command::$variant => &[ $(Action::$action,)* ], )* }
            }

            /// One line describing the capability — what task 53 hands an
            /// agent as the generated tool's description.
            #[must_use]
            pub const fn summary(self) -> &'static str {
                match self { $( Command::$variant => $summary, )* }
            }
        }
    };
}

commands! {
    // -- AccountService (task 7) ---------------------------------------------
    AccountCreate {
        rpc: "/rmail.v1.AccountService/Create",
        tool: "create_account",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Add a mail account, with its IMAP/SMTP hosts and credential source.",
    }
    AccountList {
        rpc: "/rmail.v1.AccountService/List",
        tool: "list_accounts",
        effect: Read,
        cli: [],
        actions: [],
        summary: "List the configured accounts (never their secrets).",
    }
    AccountGet {
        rpc: "/rmail.v1.AccountService/Get",
        tool: "get_account",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Read one account's settings by id.",
    }
    AccountDelete {
        rpc: "/rmail.v1.AccountService/Delete",
        tool: "delete_account",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Remove an account and the local mail belonging to it.",
    }
    // `TestConnection` stores nothing locally and still is not a `Read`: it
    // performs a real login against someone else's IMAP server, which that
    // server's rate limiter, audit log and lockout counter all observe. That
    // is an effect outside this process, which is exactly where `Effect` draws
    // its line — and the practical consequence of getting it wrong is a "safe"
    // MCP tool an agent may hammer a remote server with until it locks the
    // account out.
    AccountTestConnection {
        rpc: "/rmail.v1.AccountService/TestConnection",
        tool: "test_account_connection",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Verify an account's IMAP login and report its server capabilities.",
    }
    // Autoconfig (task 80) persists nothing, and is still `Mutate` for the
    // same reason `TestConnection` is: it reaches out to other people's
    // servers — an autoconfig fetch, a DNS lookup, and (with a credential) a
    // real login someone else's lockout counter observes — and it can spend
    // at the model provider. `Read` here would advertise an MCP tool an agent
    // could loop on until an account locked out.
    AccountAutoconfigure {
        rpc: "/rmail.v1.AccountService/Autoconfigure",
        tool: "autoconfigure_account",
        effect: Mutate,
        cli: ["account add"],
        actions: [],
        summary: "Discover an address's IMAP/SMTP settings and return a ready TOML block.",
    }
    // The OAuth trio (task 79). All three are `Mutate`, including the two that
    // sound like reads: `BeginOAuth` binds a loopback port and mints a PKCE
    // grant, and `RefreshToken` spends one use of a refresh token at the
    // provider — which on Microsoft *rotates* it, so calling it is not
    // repeatable and is very much observable outside this process.
    //
    // `mail account login --oauth <provider>` reaches Begin and Complete in
    // one verb; that is the second row in this table to claim two RPCs, and
    // `the_verbs_that_reach_two_rpcs_still_do` is the check that pins it.
    AccountBeginOAuth {
        rpc: "/rmail.v1.AccountService/BeginOAuth",
        tool: "begin_account_oauth",
        effect: Mutate,
        cli: ["account login"],
        actions: [],
        summary: "Start a loopback+PKCE OAuth2 authorization and return the URL to open.",
    }
    AccountCompleteOAuth {
        rpc: "/rmail.v1.AccountService/CompleteOAuth",
        tool: "complete_account_oauth",
        effect: Mutate,
        cli: ["account login"],
        actions: [],
        summary: "Wait for the OAuth redirect, exchange the code, and store the refresh token.",
    }
    AccountRefreshToken {
        rpc: "/rmail.v1.AccountService/RefreshToken",
        tool: "refresh_account_token",
        effect: Mutate,
        cli: ["account refresh"],
        actions: [],
        summary: "Refresh an account's OAuth access token and report its new expiry.",
    }

    // -- AdminService (task 38) ----------------------------------------------
    AdminMintToken {
        rpc: "/rmail.v1.AdminService/MintToken",
        tool: "mint_token",
        effect: Mutate,
        cli: ["token create"],
        actions: [],
        summary: "Mint a capability token, returning its bearer secret exactly once.",
    }
    AdminRevokeToken {
        rpc: "/rmail.v1.AdminService/RevokeToken",
        tool: "revoke_token",
        effect: Mutate,
        // `mail auth logout` calls this same RPC — there is no
        // `ClientAuthService.Logout` RPC of its own, since ending a session
        // is exactly "revoke the token `LoginPassword` minted" plus
        // forgetting the local cache (`rmail-cli::session`); a second RPC
        // that did the same thing under a different name would be two ways
        // to say one thing.
        cli: ["token revoke", "auth logout"],
        actions: [],
        summary: "Revoke a capability token by id.",
    }
    AdminListTokens {
        rpc: "/rmail.v1.AdminService/ListTokens",
        tool: "list_tokens",
        effect: Read,
        cli: ["token list"],
        actions: [],
        summary: "List capability tokens as metadata only — never the secret or its hash.",
    }

    // -- ClientAuthService --------------------------------------------------
    // Gates access to rmail's own API, as distinct from AccountService
    // (IMAP/SMTP credentials) or `crypto` (mail encryption) — see
    // proto/rmail/v1/client_auth.proto's module comment. SetupPassword and
    // ClearPassword sit behind `admin`, same as the AdminService rows above;
    // LoginPassword is `Requirement::SelfAuthenticated` and AuthStatus is
    // `Requirement::Public` in `rmaild::auth::methods` — both reachable with
    // no prior credential (that is the point of a login endpoint, and of a
    // status check run before deciding whether to log in), but not the same
    // guarantee: see `Requirement::SelfAuthenticated`'s own docs for why
    // LoginPassword — which mints a token, unlike AuthStatus — could not
    // just be `Public` too.
    ClientAuthSetupPassword {
        rpc: "/rmail.v1.ClientAuthService/SetupPassword",
        tool: "setup_password",
        effect: Mutate,
        cli: ["auth setup"],
        actions: [],
        summary: "Set or replace the password that gates access to rmail's own API.",
    }
    ClientAuthClearPassword {
        rpc: "/rmail.v1.ClientAuthService/ClearPassword",
        tool: "clear_password",
        effect: Mutate,
        cli: ["auth clear"],
        actions: [],
        summary: "Remove the password gate entirely.",
    }
    ClientAuthLoginPassword {
        rpc: "/rmail.v1.ClientAuthService/LoginPassword",
        tool: "login_password",
        effect: Mutate,
        cli: ["auth login"],
        actions: [],
        summary: "Prove the password and receive a session bearer token.",
    }
    ClientAuthAuthStatus {
        rpc: "/rmail.v1.ClientAuthService/AuthStatus",
        tool: "auth_status",
        effect: Read,
        cli: ["auth status"],
        actions: [],
        summary: "Report whether a password is configured and whether local callers must log in.",
    }

    // -- SyncService (task 15) -----------------------------------------------
    SyncSyncFolder {
        rpc: "/rmail.v1.SyncService/SyncFolder",
        tool: "sync_folder",
        effect: Mutate,
        cli: ["sync"],
        actions: [],
        summary: "Run a sync pass over one mailbox, or every folder of an account.",
    }
    SyncStatus {
        rpc: "/rmail.v1.SyncService/Status",
        tool: "sync_status",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Report per-folder sync progress, strategy and last error.",
    }
    SyncPause {
        rpc: "/rmail.v1.SyncService/Pause",
        tool: "pause_sync",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Stop the background sync loop; local mail stays readable.",
    }
    SyncResume {
        rpc: "/rmail.v1.SyncService/Resume",
        tool: "resume_sync",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Restart the background sync loop after a pause.",
    }
    SyncWatchEvents {
        rpc: "/rmail.v1.SyncService/WatchEvents",
        tool: "watch_sync_events",
        effect: Read,
        // `mail sync --watch` keeps streaming from where the pass it just ran
        // ended, so the one verb reaches two RPCs.
        cli: ["sync"],
        actions: [],
        summary: "Stream sync events from a cursor, resuming without gaps.",
    }

    // -- MailService (task 39) -----------------------------------------------
    MailList {
        rpc: "/rmail.v1.MailService/List",
        tool: "list_messages",
        effect: Read,
        cli: ["list"],
        actions: [],
        summary: "Stream a mailbox's messages, newest first.",
    }
    // The third row to claim a verb another row also claims: `mail list`
    // reaches `List` with `--mailbox` and `ListUnified` with `--all`, which is
    // the whole point of the flag. `the_verbs_that_reach_two_rpcs_still_do`
    // pins the pattern.
    MailListUnified {
        rpc: "/rmail.v1.MailService/ListUnified",
        tool: "list_unified_inbox",
        effect: Read,
        cli: ["list"],
        actions: [],
        summary: "Stream every account's inbox as one time-ordered, deduplicated view.",
    }
    MailGet {
        rpc: "/rmail.v1.MailService/Get",
        tool: "get_message",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Fetch one message in full, with its body and attachment metadata.",
    }
    MailGetThread {
        rpc: "/rmail.v1.MailService/GetThread",
        tool: "get_thread",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Fetch a whole conversation in reply order.",
    }
    MailMove {
        rpc: "/rmail.v1.MailService/Move",
        tool: "move_message",
        effect: Mutate,
        cli: [],
        // Archiving *is* a move, to the account's archive folder — one
        // capability, two ways to ask for it.
        actions: [Archive, MoveTo],
        summary: "Move messages to another mailbox, reflecting the move to IMAP.",
    }
    MailCopy {
        rpc: "/rmail.v1.MailService/Copy",
        tool: "copy_message",
        effect: Mutate,
        cli: [],
        actions: [CopyTo],
        summary: "Copy messages into another mailbox, leaving the originals in place.",
    }
    MailSetFlags {
        rpc: "/rmail.v1.MailService/SetFlags",
        tool: "set_flags",
        effect: Mutate,
        cli: [],
        actions: [ToggleRead, ToggleFlag],
        summary: "Add or remove IMAP flags (\\Seen, \\Flagged, ...) on a message.",
    }
    MailDelete {
        rpc: "/rmail.v1.MailService/Delete",
        tool: "delete_message",
        effect: Mutate,
        cli: [],
        actions: [Delete],
        summary: "Delete a message — an expunge, not a move to trash.",
    }
    MailGetAttachment {
        rpc: "/rmail.v1.MailService/GetAttachment",
        tool: "get_attachment",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Stream one attachment's bytes in frame-sized chunks.",
    }
    MailWatchEvents {
        rpc: "/rmail.v1.MailService/WatchEvents",
        tool: "watch_mail_events",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Stream mail events (new message, flags changed, expunged) from a cursor.",
    }

    // -- SearchService (tasks 33, 37, 51, 64) ---------------------------------
    // Every ranking RPC below is `Read`, and the reason is *not* "it only
    // reads the local index" — it is the clamp, and the clamp has to cover two
    // separate provider paths rather than the one that was written down first.
    //
    // The rerank path is the obvious one: `SearchApi::rerank_for` lets a
    // request reduce the backend `search.rerank` configures and never escalate
    // past it, so a caller cannot spend anything the operator had not already
    // turned on (`rmaild::auth::methods` argues this at length).
    //
    // The *query embedder* is the one that is easy to miss. `QueryPlanner`
    // embeds every query, and `index.semantic.provider = "voyage"` makes that
    // a metered call to a third-party API (`crate::embed::voyage`) — so on
    // such a deployment `search_mail` egresses the query text and spends, on
    // every call. It stays `Read` on the same clamp: the embedder is one
    // process-wide instance built from `[index.semantic]`, a request cannot
    // name or change it, and a caller can therefore cause nothing the operator
    // did not already configure. That is the identical argument, and it is
    // written here rather than inferred because the two paths are separate
    // code and only one of them had it.
    //
    // `Evaluate` is the sharpest case and still `Read`: it runs the pipeline
    // once per golden query, so one call is N embeddings rather than one. The
    // clamp holds (it cannot select a backend either), but the amplification
    // is real, which is why `search.eval` work belongs to an operator rather
    // than to an unattended agent loop. If the embedder ever becomes
    // request-selectable, these rows become `Mutate` and want `ai.invoke`,
    // exactly as `AiAskMailbox` does.
    SearchSearch {
        rpc: "/rmail.v1.SearchService/Search",
        tool: "search_mail",
        effect: Read,
        cli: ["search"],
        actions: [SearchOpen],
        summary: "Ranked hybrid search over the local index, streaming hits as they rank.",
    }
    SearchSemantic {
        rpc: "/rmail.v1.SearchService/Semantic",
        tool: "semantic_search",
        effect: Read,
        cli: ["similar"],
        actions: [],
        summary: "Embedding-kNN search: neighbors by meaning, with no keyword overlap needed.",
    }
    SearchExplain {
        rpc: "/rmail.v1.SearchService/Explain",
        tool: "explain_ranking",
        effect: Read,
        cli: [],
        actions: [SearchExplain],
        summary: "Re-derive why one hit ranked where it did: feature contributions and matched spans.",
    }
    // `Mutate`, unlike every other row in this service, and for the reason
    // `AiAskMailbox` is: a compile is a provider call the caller chose, not a
    // clamp-to-configured-backend read, so it spends real money and writes a
    // cache row and an audit-ledger row. Calling it `Read` would let task 53
    // project it as a safe tool an agent may call freely, once per phrasing.
    SearchCompileQuery {
        rpc: "/rmail.v1.SearchService/CompileQuery",
        tool: "compile_query",
        effect: Mutate,
        cli: ["search"],
        actions: [],
        summary: "Compile plain English into a confirmable, cached query in the operator grammar.",
    }
    SearchEvaluate {
        rpc: "/rmail.v1.SearchService/Evaluate",
        tool: "evaluate_search",
        effect: Read,
        cli: ["search eval"],
        actions: [],
        summary: "Score a golden set against the corpus and report NDCG@10, MRR, Recall@50, P@3.",
    }
    SearchLogFeedback {
        rpc: "/rmail.v1.SearchService/LogFeedback",
        tool: "log_search_feedback",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Record which results a searcher opened, as training signal for ranking.",
    }
    // `Read` on the same grounds `SearchSearch` is, and on those grounds
    // only. Its dense arm embeds the query with `index.semantic`'s configured
    // embedder, which can be a hosted one — so this is not the stronger
    // "nothing observable outside this process" case, it is the clamp case:
    // the backend is the one the operator already indexes with, a caller
    // cannot select or escalate it, and so calling this grants no spend
    // authority `Search` did not already grant. `rmaild::auth::methods` puts
    // the same argument on the same row at more length.
    SearchSearchAttachments {
        rpc: "/rmail.v1.SearchService/SearchAttachments",
        tool: "search_attachments",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Rank extracted attachment text, returning the exact attachment and page that matched.",
    }
    // -- SearchService (task 73) ----------------------------------------------
    // The unqualified `Read`: it queries `entities`/`entity_mentions` and
    // touches nothing else. No extractor runs, no model is reached, and there
    // is no configured backend for a caller to escalate — so unlike its two
    // neighbours above this row needs no clamp argument at all.
    SearchSearchEntities {
        rpc: "/rmail.v1.SearchService/SearchEntities",
        tool: "search_entities",
        effect: Read,
        cli: ["entities"],
        actions: [],
        summary: "Search extracted entities — amounts, references, tracking numbers, IBANs — and return the mail behind each hit.",
    }
    // -- SearchService (task 65) ----------------------------------------------
    // The learned ranker's lifecycle. `Mutate` for the two that can change
    // which model is live, and the reason is the broadest one in this table:
    // they change what *every* future search on this daemon returns, for
    // every caller. That is a larger effect than any single-message mutation
    // above, even though nothing here touches a message.
    //
    // `rmaild::auth::methods` puts all three behind `admin`, including the
    // read, and argues why at length there.
    SearchTrainRanker {
        rpc: "/rmail.v1.SearchService/TrainRanker",
        tool: "train_ranker",
        effect: Mutate,
        cli: ["search train"],
        actions: [],
        summary: "Distil the local click log into ranker weights and hot-swap the model only on a measured NDCG win.",
    }
    SearchListRankerModels {
        rpc: "/rmail.v1.SearchService/ListRankerModels",
        tool: "list_ranker_models",
        effect: Read,
        cli: ["search models"],
        actions: [],
        summary: "List every trained ranker model, accepted or refused, with the held-out numbers that decided it.",
    }
    SearchRollbackRanker {
        rpc: "/rmail.v1.SearchService/RollbackRanker",
        tool: "rollback_ranker",
        effect: Mutate,
        cli: ["search rollback"],
        actions: [],
        summary: "Put an earlier ranker model back, or fall back to the deterministic cold-start scorer.",
    }

    // -- AttachmentService (task 74) -------------------------------------------
    // `Mutate` for the reason `AiAskMailbox` is, and it is the same reason
    // spelled out on `Effect` itself: calling the provider *is* the RPC.
    // There is no clamp-to-configured-backend escape hatch here the way
    // `SearchSearchAttachments` above has by construction — an answer with no
    // model call is not a degraded answer, it is no answer — so a caller can
    // cause spend, which is an effect an observer outside this process sees.
    AttachmentAskAttachment {
        rpc: "/rmail.v1.AttachmentService/AskAttachment",
        tool: "ask_attachment",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Answer a question from one attachment (or the best matches for it), citing page and span.",
    }
    // `Read`, unlike its neighbour above, because spend is optional here
    // rather than the whole RPC: a workbook, a CSV and an HTML table are
    // parsed with no provider at all, and only a request that sets
    // `allow_model` on a PDF or an image reaches one. A caller cannot force
    // spend, which is the line this enum's own docs draw.
    AttachmentExtractTables {
        rpc: "/rmail.v1.AttachmentService/ExtractTables",
        tool: "extract_tables",
        effect: Read,
        cli: ["attach tables"],
        actions: [],
        summary: "Read one attachment's tables as typed rows with per-cell provenance, saying which were inferred.",
    }

    // -- AttachmentService (task 73) -------------------------------------------
    // `Mutate`, unlike `ExtractTables` next door, and the model is not why:
    // every call *stores* what it read in `invoices`, which the next
    // `ExportInvoices` returns. A read that changed what a later read answers
    // is not a read, which is the same line `ExtractExtractEvents` is on for
    // its idempotency claim. (`use_model` adds provider spend on top, but a
    // caller can leave it unset and this row would still be `Mutate`.)
    AttachmentExtractInvoice {
        rpc: "/rmail.v1.AttachmentService/ExtractInvoice",
        tool: "extract_invoice",
        effect: Mutate,
        cli: ["attach invoice"],
        actions: [],
        summary: "Detect an invoice or receipt and read vendor, number, dates, totals and line items into the invoice table, per-field provenance included.",
    }
    // `list_invoices` rather than `export_invoices` as the tool name: prd.md
    // #53 names the MCP tool that way, and a listing is what an agent calls
    // it — the CSV is one rendering of the same read.
    AttachmentExportInvoices {
        rpc: "/rmail.v1.AttachmentService/ExportInvoices",
        tool: "list_invoices",
        effect: Read,
        cli: ["invoices"],
        actions: [],
        summary: "List stored invoices, optionally rendered as CSV with each row's inferred fields named.",
    }

    // -- SavedSearchService (task 35) -----------------------------------------
    SavedSearchCreateSavedSearch {
        rpc: "/rmail.v1.SavedSearchService/CreateSavedSearch",
        tool: "create_saved_search",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Store a named query that can be re-run later.",
    }
    SavedSearchUpdateSavedSearch {
        rpc: "/rmail.v1.SavedSearchService/UpdateSavedSearch",
        tool: "update_saved_search",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Change a saved search's name or query.",
    }
    SavedSearchListSavedSearches {
        rpc: "/rmail.v1.SavedSearchService/ListSavedSearches",
        tool: "list_saved_searches",
        effect: Read,
        cli: [],
        actions: [],
        summary: "List the stored saved searches.",
    }
    SavedSearchDeleteSavedSearch {
        rpc: "/rmail.v1.SavedSearchService/DeleteSavedSearch",
        tool: "delete_saved_search",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Delete a saved search by id.",
    }
    SavedSearchRunSavedSearch {
        rpc: "/rmail.v1.SavedSearchService/RunSavedSearch",
        tool: "run_saved_search",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Run a saved search now and stream its ranked hits.",
    }
    SavedSearchCreateSmartFolder {
        rpc: "/rmail.v1.SavedSearchService/CreateSmartFolder",
        tool: "create_smart_folder",
        effect: Mutate,
        cli: ["folder new"],
        actions: [],
        summary: "Define a virtual mailbox from a plain-English predicate compiled to a query.",
    }
    // `Mutate` on both counts: it stores a folder *and* spends at the
    // provider, which is why the scope table pairs `mail.write` with
    // `ai.invoke` here and not on the row above.
    SavedSearchCompileSmartFolder {
        rpc: "/rmail.v1.SavedSearchService/CompileSmartFolder",
        tool: "create_nl_smart_folder",
        effect: Mutate,
        cli: ["folder new"],
        actions: [],
        summary: "Compile a plain-English sentence into a stored hybrid smart folder plan.",
    }
    SavedSearchListSmartFolders {
        rpc: "/rmail.v1.SavedSearchService/ListSmartFolders",
        tool: "list_smart_folders",
        effect: Read,
        cli: ["folder list"],
        actions: [],
        summary: "List the defined smart folders.",
    }
    SavedSearchDeleteSmartFolder {
        rpc: "/rmail.v1.SavedSearchService/DeleteSmartFolder",
        tool: "delete_smart_folder",
        effect: Mutate,
        cli: ["folder rm"],
        actions: [],
        summary: "Delete a smart folder definition. The mail it matched is untouched.",
    }
    SavedSearchListSmartFolderMembers {
        rpc: "/rmail.v1.SavedSearchService/ListSmartFolderMembers",
        tool: "list_smart_folder_members",
        effect: Read,
        cli: ["folder members"],
        actions: [],
        summary: "Stream the messages a smart folder currently matches.",
    }
    SavedSearchEvaluateSmartFolder {
        rpc: "/rmail.v1.SavedSearchService/EvaluateSmartFolder",
        tool: "evaluate_smart_folder",
        effect: Mutate,
        cli: ["folder eval"],
        actions: [],
        summary: "Re-evaluate a smart folder, auto-tagging and notifying on genuinely new members.",
    }

    // -- FinderService (task 59) ----------------------------------------------
    // `Find` is `Read` on the same grounds `SearchSearch` is: it reads a
    // denormalized copy of local mail, reaches no provider, and writes
    // nothing an observer outside this process could see. `RebuildIndex` is
    // `Mutate` even though it only recomputes what the mailbox already
    // implies — it rewrites a table another reader can observe, and it holds
    // the single writer connection while it does, which is an effect.
    FinderFind {
        rpc: "/rmail.v1.FinderService/Find",
        tool: "fuzzy_find",
        effect: Read,
        cli: ["find"],
        actions: [FinderOpen],
        summary: "Fuzzy-match a prompt against messages, folders, contacts, saved searches, tags and commands.",
    }
    FinderBatchAction {
        rpc: "/rmail.v1.FinderService/BatchAction",
        tool: "fuzzy_batch_action",
        effect: Mutate,
        cli: ["find"],
        actions: [],
        summary: "Archive, delete or re-flag every message in a finder selection at once.",
    }
    FinderRebuildIndex {
        rpc: "/rmail.v1.FinderService/RebuildIndex",
        tool: "rebuild_finder_index",
        effect: Mutate,
        cli: ["find"],
        actions: [],
        summary: "Re-derive the whole fuzzy-finder index from the source tables.",
    }
    FinderIndexStatus {
        rpc: "/rmail.v1.FinderService/IndexStatus",
        tool: "finder_index_status",
        effect: Read,
        cli: ["find"],
        actions: [],
        summary: "Report how complete, how large and how fresh the fuzzy-finder index is.",
    }

    // -- AnalyticsService (task 71) -------------------------------------------
    // `Read` on the same grounds `SearchSearch` is, and more plainly: the
    // report is arithmetic over headers already in the local mirror. It
    // writes nothing, reaches no provider, and cannot spend anything.
    AnalyticsGetResponseTimes {
        rpc: "/rmail.v1.AnalyticsService/GetResponseTimes",
        tool: "response_time_stats",
        effect: Read,
        cli: ["stats response-time"],
        actions: [],
        summary: "Per-contact or per-folder response-time percentiles, a rolling trend, and where you are the bottleneck.",
    }
    // `Mutate`, unlike its neighbour above and for both of the reasons
    // `AiAskMailbox`/`AttachmentAskAttachment` give: calling the provider *is*
    // the RPC — a briefing with no model call is not a degraded briefing, it
    // is no briefing — and it writes a durable `digests` row a later request
    // reads back. A caller can therefore cause spend and leave state behind,
    // which is an effect an observer outside this process sees.
    AnalyticsGenerateDigest {
        rpc: "/rmail.v1.AnalyticsService/GenerateDigest",
        tool: "generate_digest",
        effect: Mutate,
        cli: ["digest"],
        actions: [],
        summary: "Brief one window of mail as ranked markdown, clustered by topic and sender, every line citing its message-ids.",
    }

    // -- AnalyticsService (task 72) -------------------------------------------
    // All three are `Mutate`, and none of them writes a row. The line this
    // enum is drawn on is *authority*, and spend at a model provider is an
    // effect an observer outside this process sees — the same reason
    // `AiAskMailbox` and `AnalyticsGenerateDigest` above are `Mutate` while
    // `SearchSearch` is not. The clamp argument that keeps `SearchSearch` at
    // `Read` is unavailable here: the caller, not the operator, chooses
    // whether a call happens (`metrics_only`, `classify_unknown`, `narrate`),
    // and a window or a question the caller picks decides how large it is.
    //
    // `GetContactInsight` is the closest call of the three, because
    // `metrics_only = true` genuinely spends nothing. It is still `Mutate`:
    // this table annotates the *RPC*, an MCP client picks a tool before it
    // picks a field, and a "safe" tool whose safety depends on one boolean is
    // worse than an honest `Mutate`.
    AnalyticsGetContactInsight {
        rpc: "/rmail.v1.AnalyticsService/GetContactInsight",
        tool: "contact_insight",
        effect: Mutate,
        cli: ["contact"],
        actions: [],
        summary: "Volume, direction, response symmetry, cadence, topics and a decay report for one correspondent, with a Claude relationship briefing.",
    }
    AnalyticsListSubscriptions {
        rpc: "/rmail.v1.AnalyticsService/ListSubscriptions",
        tool: "list_subscriptions",
        effect: Mutate,
        cli: ["subs"],
        actions: [],
        summary: "Classify senders as newsletters, transactional or automated mail with read-rates, and report unsubscribe candidates. Never unsubscribes anything.",
    }
    AnalyticsAskAnalytics {
        rpc: "/rmail.v1.AnalyticsService/AskAnalytics",
        tool: "ask_analytics",
        effect: Mutate,
        // `mail stats ask`, not prd.md's `mail ask`: that verb was taken by
        // feature 43 (`AiService/AskMailbox`), which answers a question about
        // the *contents* of messages. Two verbs spelled the same that reach
        // different services is worse than one of them living in the `stats`
        // namespace `stats_cli` was created to hold exactly this.
        cli: ["stats ask"],
        actions: [],
        summary: "Answer a plain-English question about the mailbox with rows and a short narrative, via read-only SQL over whitelisted analytics views.",
    }

    // -- IndexService (task 24) -----------------------------------------------
    IndexStatus {
        rpc: "/rmail.v1.IndexService/Status",
        tool: "index_status",
        effect: Read,
        cli: ["index status"],
        actions: [],
        summary: "Per-stage index coverage, queue depth, embedding model and lag.",
    }
    IndexReindex {
        rpc: "/rmail.v1.IndexService/Reindex",
        tool: "reindex",
        effect: Mutate,
        // Drain, selection and embedding-backfill are modes of the one RPC;
        // the CLI spells each as its own verb.
        cli: ["index run", "index reindex", "index embed"],
        actions: [],
        summary: "Enqueue and drain indexing work, streaming progress. Never deletes anything.",
    }
    IndexRebuild {
        rpc: "/rmail.v1.IndexService/Rebuild",
        tool: "rebuild_index",
        effect: Mutate,
        cli: ["index rebuild"],
        actions: [],
        summary: "DELETE a stage's derived index and recompute it; search is degraded until it catches up.",
    }
    IndexVerify {
        rpc: "/rmail.v1.IndexService/Verify",
        tool: "verify_index",
        effect: Read,
        cli: ["index verify"],
        actions: [],
        summary: "Report drift between what the index records and what it holds. Repairs nothing.",
    }
    IndexGc {
        rpc: "/rmail.v1.IndexService/Gc",
        tool: "gc_index",
        effect: Mutate,
        cli: ["index gc"],
        actions: [],
        summary: "Delete index rows whose parent message is gone.",
    }
    IndexSetPaused {
        rpc: "/rmail.v1.IndexService/SetPaused",
        tool: "set_index_paused",
        effect: Mutate,
        cli: ["index start", "index stop"],
        actions: [],
        summary: "Stop or start the background indexing worker. Queued work stays durable.",
    }
    IndexListEntities {
        rpc: "/rmail.v1.IndexService/ListEntities",
        tool: "list_entities",
        effect: Read,
        cli: ["entities"],
        actions: [],
        summary: "List entities extracted from mail (people, amounts, dates, tracking numbers) by kind.",
    }

    // -- TagService (task 55) -------------------------------------------------
    TagAddTag {
        rpc: "/rmail.v1.TagService/AddTag",
        tool: "add_tag",
        effect: Mutate,
        cli: ["tag"],
        actions: [],
        summary: "Apply tags to a message or thread, creating them on demand.",
    }
    TagRemoveTag {
        rpc: "/rmail.v1.TagService/RemoveTag",
        tool: "remove_tag",
        effect: Mutate,
        cli: ["untag"],
        actions: [],
        summary: "Remove tags from a message or thread.",
    }
    TagListTags {
        rpc: "/rmail.v1.TagService/ListTags",
        tool: "list_tags",
        effect: Read,
        cli: ["tags"],
        actions: [],
        summary: "List an account's tags with their colors, hierarchy and message counts.",
    }
    TagCreateTag {
        rpc: "/rmail.v1.TagService/CreateTag",
        tool: "create_tag",
        effect: Mutate,
        cli: ["tags create"],
        actions: [],
        summary: "Create or update a tag definition: name, color, IMAP sync mode, parent.",
    }
    TagBulkTag {
        rpc: "/rmail.v1.TagService/BulkTag",
        tool: "bulk_tag",
        effect: Mutate,
        // Only `tag-bulk`. `mail tag search:<query>` looks like it would reach
        // this RPC and deliberately does not — it refuses and points at
        // `tag-bulk`, because the bulk form needs an account the per-message
        // form does not (see `tag_cli`'s own `ParsedTarget::Bulk` arm).
        cli: ["tag-bulk"],
        actions: [],
        summary: "Apply tags to every message a filter-only query selects, in one transaction.",
    }
    // `Mutate`, not `Read`, since task 57: this RPC streams what is already
    // pending *and then* classifies the message, which spends a model call and
    // writes `message_tags` rows (pending ones, plus any a `tag_rules` row
    // auto-applies). Task 55 declared it `Read` because its implementation
    // genuinely could not do either. Leaving it `Read` would let MCP project a
    // paid, mailbox-mutating tool as a safe one — exactly what the
    // parity/scope agreement test exists to catch.
    TagSuggestTags {
        rpc: "/rmail.v1.TagService/SuggestTags",
        tool: "suggest_tags",
        effect: Mutate,
        cli: ["suggest-tags"],
        actions: [],
        summary: "Classify a message against the tag taxonomy and stream its pending suggestions.",
    }
    TagResolveSuggestion {
        rpc: "/rmail.v1.TagService/ResolveSuggestion",
        tool: "resolve_tag_suggestion",
        effect: Mutate,
        cli: ["accept-tags", "reject-tags"],
        actions: [],
        summary: "Accept or reject a pending tag suggestion, which also trains the tagger.",
    }
    TagSetTagRule {
        rpc: "/rmail.v1.TagService/SetTagRule",
        tool: "set_tag_rule",
        effect: Mutate,
        cli: ["tag-rules set"],
        actions: [],
        summary: "Create or re-point a tag rule, which decides whether a confident suggestion applies itself.",
    }
    TagListTagRules {
        rpc: "/rmail.v1.TagService/ListTagRules",
        tool: "list_tag_rules",
        effect: Read,
        cli: ["tag-rules list"],
        actions: [],
        summary: "List an account's tag rules, enabled or not.",
    }

    // -- NoteService (task 56) ------------------------------------------------
    NoteAddNote {
        rpc: "/rmail.v1.NoteService/AddNote",
        tool: "add_note",
        effect: Mutate,
        cli: ["note add"],
        actions: [],
        summary: "Attach a freeform note to a message or a thread.",
    }
    NoteEditNote {
        rpc: "/rmail.v1.NoteService/EditNote",
        tool: "edit_note",
        effect: Mutate,
        cli: ["note edit"],
        actions: [],
        summary: "Replace a note's body.",
    }
    NoteDeleteNote {
        rpc: "/rmail.v1.NoteService/DeleteNote",
        tool: "delete_note",
        effect: Mutate,
        cli: ["note rm"],
        actions: [],
        summary: "Delete a note by id.",
    }
    NoteListNotes {
        rpc: "/rmail.v1.NoteService/ListNotes",
        tool: "list_notes",
        effect: Read,
        cli: ["notes"],
        actions: [],
        summary: "List the notes on a message or thread, newest first.",
    }
    NoteWatchNotes {
        rpc: "/rmail.v1.NoteService/WatchNotes",
        tool: "watch_notes",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Stream note additions, edits and deletions as they happen.",
    }

    // -- ComposeService (task 60) ---------------------------------------------
    ComposeCreateDraft {
        rpc: "/rmail.v1.ComposeService/CreateDraft",
        tool: "create_draft",
        effect: Mutate,
        cli: [],
        // Reply and forward are drafts with headers pre-filled from the
        // message on screen; neither sends anything.
        actions: [Reply, Forward],
        summary: "Create a draft, optionally pre-filled as a reply or forward of a message.",
    }
    ComposeGetDraft {
        rpc: "/rmail.v1.ComposeService/GetDraft",
        tool: "get_draft",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Read one draft by id.",
    }
    ComposeListDrafts {
        rpc: "/rmail.v1.ComposeService/ListDrafts",
        tool: "list_drafts",
        effect: Read,
        cli: [],
        actions: [],
        summary: "List the drafts an account holds.",
    }
    ComposeUpdateDraft {
        rpc: "/rmail.v1.ComposeService/UpdateDraft",
        tool: "update_draft",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Edit a draft's recipients, subject, body or attachments.",
    }
    ComposeDeleteDraft {
        rpc: "/rmail.v1.ComposeService/DeleteDraft",
        tool: "delete_draft",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Delete a draft by id.",
    }
    ComposeRenderDraft {
        rpc: "/rmail.v1.ComposeService/RenderDraft",
        tool: "render_draft",
        // Persists nothing, and is still not `Read`: it emits the complete
        // octets of a transmissible message, Message-ID and all, which any
        // SMTP client can put on the wire. See `Effect`'s own doc comment and
        // `rmaild::auth::methods`, which puts this row behind `mail.send` for
        // the same reason.
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Render a draft to final RFC 5322 octets without sending it.",
    }

    // -- ComposeService, AI half (task 62) ------------------------------------
    ComposeDraftReply {
        rpc: "/rmail.v1.ComposeService/DraftReply",
        tool: "draft_reply",
        // Spend at a provider is an observable effect (see `Effect`), and it
        // writes a draft row. It is still nowhere near `send`: the draft it
        // produces has to go through `SendSchedulerService` like any other.
        effect: Mutate,
        cli: ["reply"],
        actions: [],
        summary: "Draft an on-voice reply to a message with Claude, staged as an editable draft \
                  that is never sent.",
    }
    ComposeRewriteDraft {
        rpc: "/rmail.v1.ComposeService/RewriteDraft",
        tool: "rewrite_draft",
        effect: Mutate,
        cli: ["draft rewrite"],
        actions: [],
        summary: "Rewrite a draft to a target tone and length, stored as a revertible revision.",
    }
    ComposeListDraftRevisions {
        rpc: "/rmail.v1.ComposeService/ListDraftRevisions",
        tool: "list_draft_revisions",
        effect: Read,
        cli: ["draft revisions"],
        actions: [],
        summary: "List a draft's stored revisions, oldest first.",
    }
    ComposeSelectDraftRevision {
        rpc: "/rmail.v1.ComposeService/SelectDraftRevision",
        tool: "select_draft_revision",
        effect: Mutate,
        cli: ["draft revert"],
        actions: [],
        summary: "Point a draft at one of its revisions — the cycle and the revert.",
    }

    // -- SendSchedulerService (task 61) ----------------------------------------
    SendSchedulerScheduleSend {
        rpc: "/rmail.v1.SendSchedulerService/ScheduleSend",
        tool: "schedule_send",
        effect: Mutate,
        cli: ["send"],
        actions: [],
        summary: "Queue a message for transmission now (inside an undo window) or at a stated time.",
    }
    SendSchedulerCancelScheduled {
        rpc: "/rmail.v1.SendSchedulerService/CancelScheduled",
        tool: "cancel_scheduled_send",
        effect: Mutate,
        cli: ["undo", "outbox cancel"],
        actions: [OutboxCancel],
        summary: "Cancel a queued send — inside its undo window, or any time before it is due.",
    }
    SendSchedulerRescheduleSend {
        rpc: "/rmail.v1.SendSchedulerService/RescheduleSend",
        tool: "reschedule_send",
        effect: Mutate,
        cli: ["outbox reschedule"],
        actions: [],
        summary: "Move a queued send to a different time.",
    }
    SendSchedulerUpdateScheduledBody {
        rpc: "/rmail.v1.SendSchedulerService/UpdateScheduledBody",
        tool: "update_scheduled_body",
        effect: Mutate,
        cli: ["outbox edit"],
        actions: [],
        summary: "Replace the body of a message that has not gone out yet.",
    }
    SendSchedulerSendNow {
        rpc: "/rmail.v1.SendSchedulerService/SendNow",
        tool: "send_now",
        effect: Mutate,
        cli: ["outbox send-now"],
        actions: [],
        summary: "Make a scheduled message due immediately.",
    }
    SendSchedulerRetryFailed {
        rpc: "/rmail.v1.SendSchedulerService/RetryFailed",
        tool: "retry_failed_send",
        effect: Mutate,
        cli: ["outbox retry"],
        actions: [],
        summary: "Return a message the server refused to the send queue.",
    }
    SendSchedulerListOutbox {
        rpc: "/rmail.v1.SendSchedulerService/ListOutbox",
        tool: "list_outbox",
        effect: Read,
        // `outbox show` is the same listing narrowed to one id.
        cli: ["outbox", "outbox show"],
        actions: [OutboxOpen],
        summary: "List queued, sent and failed outbound mail with its state.",
    }
    SendSchedulerWatchOutbox {
        rpc: "/rmail.v1.SendSchedulerService/WatchOutbox",
        tool: "watch_outbox",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Stream outbox state changes — the undo countdown, a send, a failure.",
    }
    SendSchedulerSuggestSendTime {
        rpc: "/rmail.v1.SendSchedulerService/SuggestSendTime",
        tool: "suggest_send_time",
        effect: Read,
        cli: ["outbox suggest"],
        actions: [],
        summary: "Propose a send time inside the configured guardrails. Schedules nothing.",
    }
    SendSchedulerCreateFollowup {
        rpc: "/rmail.v1.SendSchedulerService/CreateFollowup",
        tool: "create_followup",
        effect: Mutate,
        cli: ["followup add"],
        actions: [],
        summary: "Arm a reminder to chase a sent message that has had no reply.",
    }
    SendSchedulerListFollowups {
        rpc: "/rmail.v1.SendSchedulerService/ListFollowups",
        tool: "list_followups",
        effect: Read,
        cli: ["followup list"],
        actions: [],
        summary: "List follow-up reminders and whether each is due.",
    }
    SendSchedulerDismissFollowup {
        rpc: "/rmail.v1.SendSchedulerService/DismissFollowup",
        tool: "dismiss_followup",
        effect: Mutate,
        cli: ["followup dismiss"],
        actions: [],
        summary: "Dismiss a follow-up reminder.",
    }

    // -- The pre-send guardian and the waiting-on tracker (task 63) -----------
    SendSchedulerPreflightCheck {
        rpc: "/rmail.v1.SendSchedulerService/PreflightCheck",
        tool: "preflight_check",
        // `Mutate`, by this enum's own rule rather than by intuition: spend at
        // a model provider is an effect an observer outside this process can
        // see, which is what puts `AttachmentService/AskAttachment` on this
        // side of the line too. Nothing in the mailbox changes; a bill does.
        effect: Mutate,
        // No CLI verb yet. `mail send --force` is the *override*, which
        // belongs to `ScheduleSend`; a `mail draft check` is a later task.
        cli: [],
        actions: [],
        summary: "Review a message before it is sent and report what a human should fix.",
    }
    SendSchedulerTrackFollowup {
        rpc: "/rmail.v1.SendSchedulerService/TrackFollowup",
        tool: "track_followup",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Judge whether a sent message expects a reply and arm a waiting-on entry.",
    }
    SendSchedulerListWaitingOn {
        rpc: "/rmail.v1.SendSchedulerService/ListWaitingOn",
        tool: "list_waiting_on",
        effect: Read,
        cli: [],
        actions: [],
        summary: "List what is still unanswered, longest wait first.",
    }
    SendSchedulerDraftNudge {
        rpc: "/rmail.v1.SendSchedulerService/DraftNudge",
        tool: "draft_followup",
        // `Mutate` for the reason `PreflightCheck` is, and more plainly:
        // generating text *is* the RPC, so there is no version of it that
        // does not spend. It still has no path to the outbox — see
        // `rmail_core::outbox::followup::track`'s module docs.
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Draft a follow-up nudge for a waiting-on entry. Sends nothing.",
    }

    // -- AiService (tasks 50, 52) ----------------------------------------------
    AiGetSummary {
        rpc: "/rmail.v1.AiService/GetSummary",
        tool: "get_summary",
        effect: Read,
        cli: ["ai summary"],
        actions: [AiPanel],
        summary: "Read a message's cached AI summary. Never calls the model.",
    }
    AiAnalyzeMessage {
        rpc: "/rmail.v1.AiService/AnalyzeMessage",
        tool: "analyze_message",
        effect: Mutate,
        cli: ["ai process"],
        actions: [],
        summary: "Force a fresh deep-pass analysis of one message, streaming tokens as they arrive.",
    }
    AiStreamEnrichments {
        rpc: "/rmail.v1.AiService/StreamEnrichments",
        tool: "stream_enrichments",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Stream AI enrichments as the pipeline produces them, resumable from a cursor.",
    }
    AiSuggestReply {
        rpc: "/rmail.v1.AiService/SuggestReply",
        tool: "suggest_reply",
        effect: Mutate,
        cli: ["ai reply"],
        actions: [AiQuick],
        summary: "Return a suggested reply, generating and caching one if none exists yet.",
    }
    AiGetUsage {
        rpc: "/rmail.v1.AiService/GetUsage",
        tool: "get_ai_usage",
        effect: Read,
        cli: ["ai status", "ai cost"],
        actions: [],
        summary: "Queue depth, today's and this month's tokens and cost, headroom, pause state.",
    }
    AiSetPaused {
        rpc: "/rmail.v1.AiService/SetPaused",
        tool: "set_ai_paused",
        effect: Mutate,
        cli: ["ai pause", "ai resume"],
        actions: [],
        summary: "Pause or resume the daemon's AI dispatch loop for every account it serves.",
    }
    AiRetryFailed {
        rpc: "/rmail.v1.AiService/RetryFailed",
        tool: "retry_failed_ai_jobs",
        effect: Mutate,
        cli: ["ai retry"],
        actions: [],
        summary: "Requeue every quarantined AI job across the whole daemon.",
    }
    // `Mutate` where `SearchService/Search` is `Read`, although both can reach
    // the provider — see `Effect`'s own doc comment for why the two differ.
    AiAskMailbox {
        rpc: "/rmail.v1.AiService/AskMailbox",
        tool: "ask_mailbox",
        effect: Mutate,
        cli: ["ask"],
        actions: [AskOpen],
        summary: "Answer a plain-English question over the mailbox, streaming a cited answer.",
    }

    // -- AiPolicyService (task 76) ---------------------------------------------
    AiPolicySetBudget {
        rpc: "/rmail.v1.AiPolicyService/SetBudget",
        tool: "set_ai_budget",
        effect: Mutate,
        cli: ["ai budget set"],
        actions: [],
        summary: "Set the daily/monthly token and dollar caps for one account or globally.",
    }
    AiPolicyGetSpend {
        rpc: "/rmail.v1.AiPolicyService/GetSpend",
        tool: "get_ai_budget",
        effect: Read,
        cli: ["ai budget status"],
        actions: [],
        summary: "Report spend so far today and this month against the caps in force.",
    }
    // -- AiPolicyService, backend routing (task 78) ----------------------------
    AiPolicySetAiProvider {
        rpc: "/rmail.v1.AiPolicyService/SetAiProvider",
        tool: "set_ai_provider",
        effect: Mutate,
        cli: ["ai provider set"],
        actions: [],
        summary: "Route one account's AI calls to the on-device backend or the hosted one.",
    }
    AiPolicyGetAiProvider {
        rpc: "/rmail.v1.AiPolicyService/GetAiProvider",
        tool: "get_ai_provider",
        effect: Read,
        cli: ["ai provider status"],
        actions: [],
        summary: "Report which AI backend an account uses and whether the local model is ready.",
    }

    // -- AiSafetyService (task 77) ---------------------------------------------
    AiSafetyScanInjection {
        rpc: "/rmail.v1.AiSafetyService/ScanInjection",
        tool: "scan_prompt_injection",
        effect: Read,
        cli: ["ai scan-injection"],
        actions: [],
        summary: "Scan one message for prompt-injection signals, quoting what it tried. Costs nothing.",
    }
    AiSafetyConfirmInjection {
        rpc: "/rmail.v1.AiSafetyService/ConfirmInjection",
        tool: "confirm_prompt_injection",
        effect: Mutate,
        // Reached by `mail ai scan-injection --confirm`/`--revoke`, which
        // always rescans first so a confirmation names findings just seen.
        cli: ["ai scan-injection"],
        actions: [],
        summary: "Release (or re-withhold) rule actions on a message flagged for prompt injection.",
    }

    // -- ExtractService / LinkService (task 75) --------------------------------
    ExtractExtractEvents {
        rpc: "/rmail.v1.ExtractService/ExtractEvents",
        tool: "extract_events",
        // Mutating even with the default `ics` sink, on two counts a caller
        // holds authority over: `use_model` spends at a provider, and every
        // call *claims* the items it returns in `extraction_deliveries`, which
        // is what makes the pipe and webhook sinks idempotent. A read that
        // consumed an idempotency claim would be a read that changed what the
        // next call does.
        effect: Mutate,
        cli: ["extract events"],
        actions: [],
        summary: "Extract calendar events from a message and any .ics, and deliver them once.",
    }
    ExtractExtractTasks {
        rpc: "/rmail.v1.ExtractService/ExtractTasks",
        tool: "extract_tasks",
        effect: Mutate,
        cli: ["extract tasks"],
        actions: [],
        summary: "Extract actionable tasks from a message and any .ics, and deliver them once.",
    }
    // -- ExtractService (task 73) ----------------------------------------------
    // `Mutate` on both counts this enum draws the line at: the model call is
    // the whole mechanism (there is no deterministic route to a caller-chosen
    // schema, so a caller *can* force spend), and the validated document is
    // stored under `(message, schema)` for later reads.
    ExtractExtractStructured {
        rpc: "/rmail.v1.ExtractService/ExtractStructured",
        tool: "extract_data",
        effect: Mutate,
        cli: ["extract data"],
        actions: [],
        summary: "Read one message against a named or supplied JSON schema, validate the answer, and store it.",
    }
    LinkExtractLinks {
        rpc: "/rmail.v1.LinkService/ExtractLinks",
        tool: "extract_links",
        // A read despite `use_model`: unlike `AiService/AskMailbox`, a model
        // call here is a *refinement* of an answer the daemon produces
        // deterministically either way, so a caller cannot obtain anything by
        // spending that it could not obtain without — the same line
        // `SearchService/Search` is on.
        effect: Read,
        cli: ["links"],
        actions: [],
        summary: "Extract, deduplicate and rank a message's links, flagging targets that misrepresent themselves.",
    }

    // -- AgentService (task 69) ------------------------------------------------
    // The most consequential pair in this table. `RunInboxAgent` is the only
    // capability in the product that mutates a mailbox with no human in the
    // loop, driven by a model reading attacker-authored text.
    //
    // `Mutate` unconditionally, including for the dry run that is its default.
    // Two independent reasons, and either alone would settle it:
    //
    //  - It spends at a provider on every iteration, which is the effect
    //    outside this process that separates `AiService/AskMailbox` from
    //    `SearchService/Search` in this enum's own doc comment. A dry run
    //    spends exactly as much as a live one.
    //  - `Effect` is what task 53 gates MCP tools by, and this row's whole job
    //    is to make sure the projection never advertises "run the inbox agent"
    //    as safe. A `Read` here would hand an agent a tool it could loop on —
    //    unattended spend, and one `mutate: true` away from unattended
    //    mutation.
    //
    // The scope table (`rmaild::auth::methods`) draws the finer line the
    // effect enum cannot: `mail.read` + `mail.write` + `ai.invoke` +
    // `automation`, all four, because this RPC exercises every one of those
    // authorities at once.
    AgentRunInboxAgent {
        rpc: "/rmail.v1.AgentService/RunInboxAgent",
        tool: "run_inbox_agent",
        effect: Mutate,
        cli: ["agent run"],
        actions: [],
        summary: "Walk a mailbox once, deciding one of archive/label/snooze/draft-reply/escalate per message; dry-run unless asked to mutate.",
    }
    // A read: it returns rows the agent already wrote, calls no model, and
    // consumes no idempotency claim. The nearest neighbour is
    // `AuditService`'s ledger read, which is on the same line.
    AgentGetAgentRunLog {
        rpc: "/rmail.v1.AgentService/GetAgentRunLog",
        tool: "get_agent_run_log",
        effect: Read,
        cli: ["agent log"],
        actions: [],
        summary: "Read what the inbox agent did, run by run, with the reason it gave for each action.",
    }

    // -- WebhookService (task 68) ----------------------------------------------
    WebhookRegister {
        rpc: "/rmail.v1.WebhookService/Register",
        tool: "register_webhook",
        effect: Mutate,
        cli: ["webhook add"],
        actions: [],
        summary: "Register an outbound endpoint that receives HMAC-signed JSON about mail events.",
    }
    WebhookList {
        rpc: "/rmail.v1.WebhookService/List",
        tool: "list_webhooks",
        effect: Read,
        cli: ["webhook list"],
        actions: [],
        summary: "List the registered outbound destinations (never their signing keys).",
    }
    WebhookRemove {
        rpc: "/rmail.v1.WebhookService/Remove",
        tool: "remove_webhook",
        effect: Mutate,
        cli: ["webhook rm"],
        actions: [],
        summary: "Remove an outbound destination and its delivery history.",
    }
    WebhookSetEnabled {
        rpc: "/rmail.v1.WebhookService/SetEnabled",
        tool: "set_webhook_enabled",
        effect: Mutate,
        cli: ["webhook enable", "webhook disable"],
        actions: [],
        summary: "Stop or resume sending to an outbound destination, keeping it and its history.",
    }
    WebhookListDeliveries {
        rpc: "/rmail.v1.WebhookService/ListDeliveries",
        tool: "list_webhook_deliveries",
        effect: Read,
        cli: ["webhook deliveries"],
        actions: [],
        summary: "Inspect the outbound delivery queue: what was sent, what is retrying, what gave up.",
    }
    WebhookReplayDelivery {
        rpc: "/rmail.v1.WebhookService/ReplayDelivery",
        tool: "replay_webhook_delivery",
        // Mutating on the strongest reading of `Effect`: it causes the same
        // mail content to be POSTed to a third party again. That the row it
        // edits is a queue entry is beside the point — the authority here is
        // egress, which is why this sits behind the same scopes `Forward`
        // does rather than the read scopes `ListDeliveries` needs.
        effect: Mutate,
        cli: ["webhook replay"],
        actions: [],
        summary: "Re-arm one outbound delivery for another attempt, resending the frozen body.",
    }
    WebhookForward {
        rpc: "/rmail.v1.WebhookService/Forward",
        tool: "forward_message",
        effect: Mutate,
        cli: ["forward"],
        actions: [],
        summary: "Queue one message to a named destination as a summary + action items + deep link.",
    }

    // -- AuditService (task 45) ------------------------------------------------
    AuditQueryAiCalls {
        rpc: "/rmail.v1.AuditService/QueryAiCalls",
        tool: "query_ai_audit",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Query the append-only ledger of every model call: model, tokens, cost, payload hash.",
    }
    AuditExportLedger {
        rpc: "/rmail.v1.AuditService/ExportLedger",
        tool: "export_ai_audit",
        effect: Read,
        cli: [],
        actions: [],
        summary: "Stream the whole AI audit ledger for export.",
    }

    // -- ExportService (task 82) -----------------------------------------------
    ExportExport {
        rpc: "/rmail.v1.ExportService/Export",
        tool: "export_messages",
        // A read despite writing a file: the file is written by the *client*
        // from the stream this RPC returns. Calling it changes nothing an
        // observer outside the caller's own process could see, and it cannot
        // spend at a provider — `with_ai` attaches artifacts the AI passes
        // already stored (see `crate::export`'s module docs).
        effect: Read,
        cli: ["export"],
        actions: [],
        summary: "Export a query or thread to mbox, Maildir, .eml, or JSON, preserving raw RFC822.",
    }

    // -- RuleService (task 66) -------------------------------------------------
    RuleCreateRule {
        rpc: "/rmail.v1.RuleService/CreateRule",
        tool: "create_rule",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Persist a classification rule the background evaluator then fires unattended.",
    }
    RuleListRules {
        rpc: "/rmail.v1.RuleService/ListRules",
        tool: "list_rules",
        effect: Read,
        cli: [],
        actions: [],
        summary: "List the configured rules and their predicates and actions.",
    }
    RuleEvaluateRules {
        rpc: "/rmail.v1.RuleService/EvaluateRules",
        tool: "run_rules_on_query",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Run rules over the messages a query selects and fire their actions.",
    }
    // Both are `Mutate` despite acting on no mail, because `Effect`'s line is
    // drawn at what a caller can cause outside this process and both spend at
    // the provider — synthesis always, a backtest for every `claude_is`
    // verdict the cache does not already hold — and both write that cache and
    // an audit-ledger row. `rule.proto`'s own header says so in as many words
    // ("not side-effect free in the absolute sense ... spends real money at
    // the provider. What it never does is act on mail"), which is why the
    // scope table asks for `ai.invoke` here. Calling them `Read` would let
    // task 53 project them as safe tools an agent may call freely, and each
    // call costs money.
    RuleSynthesizeRule {
        rpc: "/rmail.v1.RuleService/SynthesizeRule",
        tool: "synthesize_rule",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Turn a plain-English instruction into a concrete rule. Stores no rule, \
                  but calls the model.",
    }
    RuleBacktestRule {
        rpc: "/rmail.v1.RuleService/BacktestRule",
        tool: "backtest_rule",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Dry-run a rule over history: what it would have hit, why, and at what cost.",
    }
    RuleRecordCorrection {
        rpc: "/rmail.v1.RuleService/RecordCorrection",
        tool: "record_rule_correction",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Record that a rule got a message wrong, as a few-shot example for next time.",
    }

    // -- HookService (task 67) -------------------------------------------------
    HookListHooks {
        rpc: "/rmail.v1.HookService/ListHooks",
        tool: "list_hooks",
        effect: Read,
        cli: ["hook list"],
        actions: [],
        summary: "List the configured event hooks, enabled or not.",
    }
    HookTestHook {
        rpc: "/rmail.v1.HookService/TestHook",
        tool: "test_hook",
        effect: Mutate,
        cli: ["hook test"],
        actions: [],
        summary: "Run one configured hook now against a sample or supplied event.",
    }

    // -- NotificationService (task 81) -----------------------------------------
    NotificationScoreMessage {
        rpc: "/rmail.v1.NotificationService/ScoreMessage",
        tool: "score_message",
        // Mutating, not read: an unscored message is *enqueued* for scoring by
        // this call, which is spend at a provider — the same line
        // `AiAskMailbox` is on, and for the same reason.
        effect: Mutate,
        cli: ["notify score"],
        actions: [],
        summary: "Report the notification decision for a message, queueing a scoring pass if there is none.",
    }
    NotificationStreamAlerts {
        rpc: "/rmail.v1.NotificationService/StreamAlerts",
        tool: "stream_alerts",
        effect: Read,
        cli: ["notify watch"],
        actions: [],
        summary: "Stream priority notifications as they fire, resumable from a cursor.",
    }

    // -- ConfigService (task 84) -----------------------------------------------
    ConfigGetKeymap {
        rpc: "/rmail.v1.ConfigService/GetKeymap",
        tool: "get_keymap",
        effect: Read,
        // Deliberately not `mail keys list`: that reads `keys.toml` directly,
        // because bindings belong to the terminal in front of the user rather
        // than to the daemon (see `rmail-cli`'s `keys_cli` module docs).
        cli: [],
        actions: [],
        summary: "Read the effective key bindings and the action id each chord runs.",
    }
    ConfigSetBinding {
        rpc: "/rmail.v1.ConfigService/SetBinding",
        tool: "set_key_binding",
        effect: Mutate,
        cli: [],
        actions: [],
        summary: "Bind or unbind a chord in one mode.",
    }
}

impl Command {
    /// The capability an RPC path names, or `None` if it names none.
    ///
    /// The join task 53's projection walks: descriptor set → path → row.
    /// `None` cannot happen for a method this workspace actually serves —
    /// `every_rpc_in_the_descriptor_set_has_a_command` fails the suite first —
    /// so a caller may treat it as "not one of ours" rather than as a case to
    /// invent a policy for.
    #[must_use]
    pub fn for_rpc(rpc: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.rpc() == rpc)
    }

    /// The capability an MCP tool name refers to, or `None`. The reverse of
    /// [`Command::tool`], for dispatching a tool call back onto an RPC.
    #[must_use]
    pub fn for_tool(tool: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.tool() == tool)
    }

    /// Every capability a TUI action reaches. Empty for a UI-local action
    /// (see [`LOCAL_ACTIONS`]).
    pub fn for_action(action: Action) -> impl Iterator<Item = Self> {
        Self::ALL
            .iter()
            .copied()
            .filter(move |c| c.actions().contains(&action))
    }

    /// Every capability a `mail` subcommand path reaches, e.g. `"ai pause"`.
    /// Empty for a deliberately client-side verb (see [`LOCAL_CLI`]).
    pub fn for_cli(path: &str) -> impl Iterator<Item = Self> + '_ {
        Self::ALL
            .iter()
            .copied()
            .filter(move |c| c.cli().contains(&path))
    }

    /// The service part of [`Command::rpc`], e.g. `rmail.v1.MailService`.
    #[must_use]
    pub fn service(self) -> &'static str {
        split_rpc(self.rpc()).0
    }

    /// The method part of [`Command::rpc`], e.g. `Get`.
    #[must_use]
    pub fn method(self) -> &'static str {
        split_rpc(self.rpc()).1
    }
}

/// `/pkg.Service/Method` → `("pkg.Service", "Method")`.
///
/// Total rather than fallible: `every_rpc_path_is_well_formed` asserts every
/// row has both halves, so there is no malformed-path case for callers to
/// handle, and returning `("", "")` for one keeps [`Command::service`]
/// infallible instead of forcing an `unwrap` at every call site.
fn split_rpc(rpc: &'static str) -> (&'static str, &'static str) {
    match rpc.strip_prefix('/').and_then(|r| r.split_once('/')) {
        Some((service, method)) => (service, method),
        None => ("", ""),
    }
}

/// `mail` verbs that deliberately have no RPC, and why.
///
/// This is the escape hatch on "if the CLI can do it, gRPC can do it", and it
/// is written by hand on purpose: adding a `mail` subcommand fails
/// `every_cli_command_is_backed_by_a_capability` by name, and the author then
/// has to decide — in a diff a reviewer reads — whether the verb belongs in
/// [`Command`] or here. What must never happen is a verb quietly landing in
/// neither, which is the state every surface was in before this table existed.
///
/// The entries below are three arguments between them:
///
/// - `ping` is the standard `grpc.health.v1.Health/Check` probe, served by
///   `tonic-health`. It is an RPC — just not a `rmail.v1` one, so it is not in
///   this workspace's descriptor set and cannot be a [`Command`] row.
/// - `tui` *is* a client, and so is `mcp serve` (task 53). Both are adapters
///   over this table rather than entries in it — and `mcp serve` is the
///   sharpest case, because what it serves is precisely [`Command::tool`] for
///   every row here. A `Command` variant for it would have to claim an RPC
///   that projects the projection.
/// - `keys …` and `hook add` edit the user's own files (`keys.toml`, the
///   master TOML). Both have module docs in `rmail-cli` explaining why a
///   daemon round-trip would end up editing the same file anyway, with a
///   second writer to keep in sync. Note `ConfigService/GetKeymap` and
///   `SetBinding` still exist as capabilities — for the palette and for MCP —
///   they are simply not what `mail keys` uses.
pub const LOCAL_CLI: &[(&str, &str)] = &[
    (
        "ping",
        "the standard grpc.health.v1.Health/Check probe, served by tonic-health rather than \
         declared in proto/rmail/v1",
    ),
    (
        "tui",
        "the terminal UI itself — a client of this table, not an entry in it",
    ),
    (
        "mcp serve",
        "the MCP adapter itself — it serves `tool()` for every row in this table, so a row of \
         its own would be a capability that projects the projection",
    ),
    (
        "keys list",
        "reads keys.toml directly; bindings belong to the terminal, not the daemon",
    ),
    (
        "keys set",
        "rewrites keys.toml directly; see keys_cli's module docs",
    ),
    (
        "keys unset",
        "rewrites keys.toml directly; see keys_cli's module docs",
    ),
    (
        "keys actions",
        "prints the compiled-in action registry; nothing to ask a daemon",
    ),
    (
        "hook add",
        "appends a [[hooks.hooks]] block to the operator's own config file; there is no \
         CreateHook RPC by design",
    ),
    (
        "daemon start",
        "spawns the rmaild process; a capability row would have to name an RPC served by the \
         daemon that is not running yet",
    ),
    (
        "daemon status",
        "the grpc.health.v1.Health/Check probe again, plus whether anything is listening at all \
         — neither is a rmail.v1 method",
    ),
    (
        "daemon stop",
        "signals the process this machine's `mail daemon start` recorded; there is deliberately \
         no Shutdown RPC, since a capability to stop the daemon would be reachable by MCP",
    ),
    (
        "api ping",
        "grpc.health.v1.Health/Check with a latency number, the same argument as `ping`",
    ),
    (
        "api reflect",
        "grpc.reflection.v1.ServerReflection, which is served by tonic-reflection rather than \
         declared in proto/rmail/v1",
    ),
    (
        "api call",
        "the generic client itself — it reaches *every* row in this table by name, so a row of \
         its own would be a capability that claims all the others (the same argument as \
         `mcp serve`)",
    ),
];

/// TUI actions that are not capabilities, and why.
///
/// The mirror of [`LOCAL_CLI`] for `keys.toml`'s action-id vocabulary
/// (task 84's `actions!` registry). Two different arguments live here, and
/// only the first is the obvious one.
///
/// **Movement and rendering.** `cursor.*`, `focus.*`, `open`, `back`,
/// `cancel`, `quit`, `help`, `visual.*` change where the cursor is or what is
/// on screen. Several *cause* a read — opening a folder lists its messages —
/// but the action is "show me this", not a capability of the API, and treating
/// it as one would put `cursor.down` in an MCP tool list.
/// `message.open-html` belongs here too, though for its own reason: it writes
/// the HTML part to a temp file and hands it to the user's browser, a local
/// effect on the machine running the TUI with no RPC because there is nothing
/// for the daemon to do.
///
/// **Overlay completions.** `pick.accept`, `confirm.accept` and
/// `input.submit` are the second argument, and it is worth stating precisely
/// because these three are the keystrokes that actually *dispatch* a
/// mutation: `pick.accept` emits the Move or Copy, `confirm.accept` emits the
/// expunge, and `input.submit` completes a forward into a draft. They are
/// still not capabilities, because **what they complete depends on which
/// overlay is up** — `confirm.accept` means "yes" and today the only thing it
/// confirms is a delete. Naming a capability requires knowing the question.
/// The capability is declared on the action that *asked* it —
/// `message.delete` on [`Command::actions`] of `MailDelete`, `message.move`/
/// `message.copy` on `MailMove`/`MailCopy`, `message.forward` on
/// `ComposeCreateDraft` — so nothing the TUI can reach is undeclared; a
/// two-keystroke interaction is declared once, on the keystroke that says
/// what it is for.
///
/// Task 85's overlays follow that second rule exactly. `prompt.accept` and
/// `menu.accept` are the keystrokes that dispatch a search, a jump, a
/// question or a cancel, and they are local for the same reason
/// `confirm.accept` is: what they complete depends on which overlay is up.
/// The capability sits on the key that opened it — `search` on
/// `SearchSearch`, `finder` on `FinderFind`, `ask` on `AiAskMailbox`,
/// `outbox`/`outbox.cancel` on the two scheduler rows, `ai.panel` on
/// `AiGetSummary`, and `ai.quick` on `AiSuggestReply` because `.` is the only
/// key in the TUI that can reach a paid reply suggestion. `palette` is local
/// like `help`: it opens a list of the other actions and performs none.
pub const LOCAL_ACTIONS: &[Action] = &[
    Action::CursorDown,
    Action::CursorUp,
    Action::CursorTop,
    Action::CursorBottom,
    Action::FocusToggle,
    Action::FocusFolders,
    Action::FocusMessages,
    Action::Open,
    Action::Back,
    Action::Cancel,
    Action::Quit,
    Action::Help,
    // Task 103's manual. Local for the reason `help` is, only more so: every
    // page is `include_str!`-compiled and every generated section is read out
    // of `Keymap`/`command::registry`/[`Command::ALL`] in this process, so the
    // manual has no RPC to declare and works with the daemon stopped —
    // which is most of the point of a manual.
    Action::ManualOpen,
    Action::ManualBack,
    Action::ManualForward,
    Action::ManualNext,
    Action::ManualPrev,
    Action::ManualGrep,
    Action::VisualToggle,
    Action::VisualSwapEnds,
    Action::OpenHtml,
    Action::PickAccept,
    Action::ConfirmAccept,
    Action::InputSubmit,
    Action::InputBackspace,
    Action::PaletteOpen,
    // Task 89's command line, local for the same reason the palette it
    // replaces is: opening it reaches nothing. What is *typed* into it
    // reaches whatever the dispatched verb reaches, and that verb's own row
    // is what declares it — a `:` that claimed every capability at once
    // would make this table say nothing.
    Action::CommandOpen,
    Action::PromptAccept,
    Action::PromptComplete,
    Action::MenuAccept,
    // Task 90's `r`. Local for the same reason `command` is: re-running a
    // report reaches whatever that report's own verb reaches, and that verb's
    // row is what declares it. One action claiming every reportable capability
    // would make this table say nothing about any of them.
    Action::ReportRerun,
    // Task 95's `n`, local for exactly the same reason: it runs whatever the
    // highlighted row's own verb reaches.
    Action::ReportReject,
];
