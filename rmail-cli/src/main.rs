//! The `mail` CLI — a thin gRPC client for the rmail daemon.

mod analytics_cli;
mod digest_cli;
mod export_cli;
mod extract_cli;
mod find_cli;
mod folder_cli;
mod hook_cli;
mod index_cli;
mod keymap;
mod keys_cli;
mod mcp_cli;
mod note_cli;
mod notify_cli;
mod outbox_cli;
/// The CLI half of the feature-parity drift check (task 41). Test-only: it
/// asserts about `Cli`'s own `clap` tree rather than contributing to it.
#[cfg(test)]
mod parity;
mod reply_cli;
mod search_cli;
mod stats_cli;
mod tag_cli;
mod tui;
mod webhook_cli;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use export_cli::ExportArgs;
use find_cli::FindArgs;
use note_cli::{NoteAction, NotesArgs};
use outbox_cli::{FollowupAction, OutboxArgs, SendArgs, UndoArgs};
use rmail_core::socket_path_from_env;
use rmail_proto::v1::account_service_client::AccountServiceClient;
use rmail_proto::v1::admin_service_client::AdminServiceClient;
use rmail_proto::v1::ai_policy_service_client::AiPolicyServiceClient;
use rmail_proto::v1::ai_safety_service_client::AiSafetyServiceClient;
use rmail_proto::v1::ai_service_client::AiServiceClient;
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::{
    analyze_event, ask_chunk, AiProviderKind, AnalyzeMessageRequest, AskRequest, BeginOAuthRequest,
    BudgetCaps, BudgetClass, BudgetWindowCaps, Citation, ClassSpend, CompleteOAuthRequest,
    ConfirmInjectionRequest, EventKind, GetAiProviderRequest, GetSpendRequest, GetSummaryRequest,
    GetUsageRequest, InjectionSeverity, ListTokensRequest, MintTokenRequest, RefreshTokenRequest,
    RetryFailedRequest, RevokeTokenRequest, ScanInjectionRequest, ScanInjectionResponse,
    SetAiProviderRequest, SetBudgetRequest, SetPausedRequest, SuggestReplyRequest, Summary,
    SyncFolderRequest, SyncMode, WatchEventsRequest,
};
use search_cli::{SearchArgs, SimilarArgs};
use tag_cli::{TagArgs, TagsArgs, UntagArgs};
use tokio_stream::StreamExt;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;

/// rmail command-line client.
#[derive(Debug, Parser)]
#[command(name = "mail", version, about = "rmail command-line client")]
struct Cli {
    /// Path to the rmaild gRPC Unix domain socket (defaults to $RMAIL_SOCKET).
    #[arg(long, global = true, env = rmail_core::SOCKET_ENV)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Round-trip a gRPC health check against rmaild.
    Ping,
    /// Synchronize an account's mail.
    Sync {
        /// Account to sync.
        #[arg(long)]
        account: i64,
        /// Sync only this mailbox (default: every folder, priority order).
        #[arg(long)]
        mailbox: Option<i64>,
        /// Force the full UID-window walk instead of a delta pass.
        #[arg(long)]
        full: bool,
        /// After syncing, follow the event stream until interrupted.
        #[arg(long)]
        watch: bool,
    },
    /// List a mailbox, or every account's inbox at once
    /// (`MailService.List`/`MailService.ListUnified`).
    List(ListArgs),
    /// Account credentials (`AccountService.BeginOAuth/CompleteOAuth/RefreshToken`).
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    /// Manage capability tokens (`AdminService.MintToken/RevokeToken/ListTokens`).
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// The terminal UI: folders, message list, preview (`tui`).
    Tui(tui::TuiArgs),
    /// Inspect and rebind the TUI's keys (`keys.toml`; see `keys_cli`'s own
    /// module docs on why this edits a file rather than calling an RPC).
    Keys {
        #[command(subcommand)]
        action: keys_cli::KeysAction,
    },
    /// Ranked search over the local index (`SearchService.Search`).
    ///
    /// `--nl` compiles a plain-English question into a query first
    /// (`SearchService.CompileQuery`) and prints the plan before running it.
    Search(SearchArgs),
    /// Smart folders: virtual mailboxes whose membership is a live predicate
    /// (`SavedSearchService`).
    ///
    /// `mail folder new "<plain English>"` has Claude compile the sentence
    /// once into a stored hybrid plan; `--predicate` defines the same folder
    /// from operators and reaches no model.
    Folder {
        #[command(subcommand)]
        action: folder_cli::FolderAction,
    },
    /// Jump to anything by name — messages, folders, contacts, saved
    /// searches, tags, commands (`FinderService.Find`).
    ///
    /// The known-item complement to `mail search`: search ranks by
    /// relevance over message bodies, the finder matches short labels
    /// as-you-type over an in-memory index.
    Find(FindArgs),
    /// Embedding-kNN neighbors of a message (`SearchService.Semantic`).
    Similar(SimilarArgs),
    /// AI pipeline verbs (`AiService`).
    Ai {
        #[command(subcommand)]
        action: AiAction,
    },
    /// Ask a question about your mail and get a cited answer
    /// (`AiService.AskMailbox`).
    Ask(AskArgs),
    /// Add/edit/delete a note on a message or thread (`NoteService`).
    Note {
        #[command(subcommand)]
        action: NoteAction,
    },
    /// List notes on a message or thread (`NoteService.ListNotes`).
    Notes(NotesArgs),
    /// Export a query or thread to mbox / Maildir / .eml / JSON
    /// (`ExportService.Export`).
    ///
    /// An archive, not a search result: every message the selection matches
    /// is written, in a deterministic order, with its raw RFC822 preserved
    /// byte for byte.
    Export(ExportArgs),
    /// Event hooks: config-driven shell commands on mail events
    /// (`HookService`; `add` edits the local config file directly — see
    /// `hook_cli`'s own module docs).
    Hook {
        #[command(subcommand)]
        action: hook_cli::HookAction,
    },
    /// Outbound webhooks: register, list, inspect and replay deliveries
    /// (`WebhookService`).
    ///
    /// The only surface in `mail` that puts your mail on somebody else's
    /// server. Nothing is sent until a destination exists and
    /// `webhooks.enabled` is on; the default payload is sender, subject and a
    /// deep link, never the body, unless that destination was registered with
    /// `--include-body`.
    Webhook {
        #[command(subcommand)]
        action: webhook_cli::WebhookAction,
    },
    /// Post one message to a registered webhook destination as a summary +
    /// action items + deep link (`WebhookService.Forward`).
    ///
    /// Not a mail forward: nothing is transmitted to a mail recipient. This
    /// queues a notification to a chat channel or a ticketing endpoint the
    /// operator registered.
    Forward(webhook_cli::ForwardArgs),
    /// Index maintenance: coverage, drain, verify, gc, rebuild
    /// (`IndexService`).
    Index {
        #[command(subcommand)]
        action: index_cli::IndexAction,
    },
    /// Entities extracted from mail, by kind (`IndexService.ListEntities`).
    Entities(index_cli::EntitiesArgs),
    /// Apply one or more tags to a message, thread, or bulk selection
    /// (`TagService.AddTag`/`BulkTag`).
    Tag(TagArgs),
    /// Bulk-apply tags to every message a filter-only query selects
    /// (`TagService.BulkTag`).
    #[command(name = "tag-bulk")]
    TagBulk {
        /// Filter-only query (`from:`/`to:`/`subject:`/`is:`/`in:`/
        /// `has:attachment`/`tag:`).
        #[arg(long)]
        query: String,
        #[arg(long)]
        account: i64,
        /// Tag name(s) to apply.
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// Remove one or more tags from a message or thread
    /// (`TagService.RemoveTag`).
    Untag(UntagArgs),
    /// List tags, or create one (`TagService.ListTags`/`CreateTag`).
    Tags(TagsArgs),
    /// Suggest tags for a message (`TagService.SuggestTags`): prints what is
    /// already pending, then classifies the message and prints each new
    /// suggestion as it lands. Mail you have already tagged is left alone.
    #[command(name = "suggest-tags")]
    SuggestTags {
        /// Message id.
        message_id: i64,
    },
    /// Accept pending suggestions by id, as printed by `suggest-tags`
    /// (`TagService.ResolveSuggestion`).
    #[command(name = "accept-tags")]
    AcceptTags {
        #[arg(required = true)]
        message_tag_ids: Vec<i64>,
    },
    /// Reject pending suggestions by id, as printed by `suggest-tags`
    /// (`TagService.ResolveSuggestion`).
    #[command(name = "reject-tags")]
    RejectTags {
        #[arg(required = true)]
        message_tag_ids: Vec<i64>,
    },
    /// Tag rules — which tags a confident suggestion may apply by itself
    /// (`TagService.SetTagRule/ListTagRules`).
    ///
    /// Without a rule at `--mode auto`, every AI suggestion stays pending
    /// for you to accept or reject. That is the safe default, not an
    /// oversight.
    #[command(name = "tag-rules")]
    TagRules {
        #[command(subcommand)]
        action: TagRuleAction,
    },
    /// Draft an on-voice reply to a message with Claude
    /// (`ComposeService.DraftReply`).
    ///
    /// Reads the whole local thread plus samples of how you have written to
    /// this correspondent before, streams the reply as it is written, and
    /// stages it as an ordinary editable draft. It never sends: putting it on
    /// the wire is `mail send`, past the pre-send guardian.
    Reply(reply_cli::ReplyArgs),
    /// Rewrite, list and revert draft revisions (`ComposeService`).
    Draft {
        #[command(subcommand)]
        action: reply_cli::DraftAction,
    },
    /// Send a message now (undoable) or schedule it for later
    /// (`SendSchedulerService.ScheduleSend`).
    Send(SendArgs),
    /// Cancel a send inside its undo window, or any scheduled message
    /// (`SendSchedulerService.CancelScheduled`).
    Undo(UndoArgs),
    /// Inspect and manage the outbox (`SendSchedulerService`).
    Outbox(OutboxArgs),
    /// Follow-up reminders on sent mail (`SendSchedulerService`).
    Followup {
        #[command(subcommand)]
        action: FollowupAction,
    },
    /// Priority notifications: follow what fired, or ask why a message did
    /// not (`NotificationService`; thresholds live in the config file — see
    /// `notify_cli`'s own module docs).
    Notify {
        #[command(subcommand)]
        action: notify_cli::NotifyAction,
    },
    /// Mailbox analytics: response-time percentiles, trend and bottlenecks,
    /// and plain-English questions (`AnalyticsService`).
    Stats {
        #[command(subcommand)]
        action: stats_cli::StatsAction,
    },
    /// Everything one correspondence looks like — volume, direction, response
    /// symmetry, cadence, topics and a decay report — plus, with `--insight`,
    /// a Claude relationship briefing (`AnalyticsService.GetContactInsight`).
    Contact(analytics_cli::ContactArgs),
    /// Which senders are broadcasting at you, how much you read, and which are
    /// worth leaving (`AnalyticsService.ListSubscriptions`).
    ///
    /// Reports what each sender's own `List-Unsubscribe` header says and stops
    /// there: rmail never opens the URL and never sends the mail. See
    /// `analytics_cli`'s module docs.
    Subs(analytics_cli::SubsArgs),
    /// A ranked markdown briefing over one window of mail, every line citing
    /// the messages it came from (`AnalyticsService.GenerateDigest`). Reads
    /// back an existing briefing for the same window rather than paying for a
    /// second one — see `digest_cli`'s own module docs.
    Digest(digest_cli::DigestArgs),
    /// Attachment verbs (`AttachmentService`).
    Attach {
        #[command(subcommand)]
        action: extract_cli::AttachAction,
    },
    /// Invoices and receipts already extracted, newest first
    /// (`AttachmentService.ExportInvoices`).
    ///
    /// A read over the stored table: it extracts nothing and calls no model,
    /// so it lists only what `mail attach invoice` has already read.
    /// `--export csv` writes an RFC 4180 document on stdout whose
    /// `inferred_fields` column names every field on the row a model inferred.
    Invoices(extract_cli::InvoicesArgs),
    /// Calendar events, tasks, and schema-shaped data out of a message
    /// (`ExtractService`). Delivery is idempotent per message: running this
    /// twice does not create the reminder twice.
    Extract {
        #[command(subcommand)]
        action: extract_cli::ExtractAction,
    },
    /// A message's links, deduplicated and ranked, with the ones whose text
    /// misrepresents their target flagged (`LinkService.ExtractLinks`).
    ///
    /// Nothing here opens or resolves a link — see `extract_cli`'s own module
    /// docs on why there is no `--open`.
    Links(extract_cli::LinksArgs),
    /// Serve the whole gRPC surface to an AI agent over the Model Context
    /// Protocol. The tool list is generated from the compiled service
    /// definitions, so it is never out of step with the API — see
    /// `mcp_cli`'s own module docs, and read the note there on `--scope`
    /// before pointing an agent at this.
    Mcp {
        #[command(subcommand)]
        action: mcp_cli::McpAction,
    },
}

/// `mail ask "<question>"` — retrieval-augmented question answering over the
/// local mailbox.
#[derive(Debug, clap::Args)]
struct AskArgs {
    /// The question, in plain English.
    question: String,
    /// Restrict retrieval to one account (default: every configured account).
    #[arg(long)]
    account: Option<i64>,
    /// Extra filter terms, in the same operator DSL `mail search` uses
    /// (`in:`, `from:`, `after:` ...).
    #[arg(long, default_value = "")]
    filter: String,
    /// How many messages to retrieve before packing (default: `ai.ask.top_k`).
    #[arg(long)]
    top_k: Option<u32>,
    /// Print what retrieval found before the answer starts.
    #[arg(long)]
    trace: bool,
}

#[derive(Debug, Subcommand)]
enum AiAction {
    /// Queue depth, today's tokens/cost, headroom, and pause state
    /// (`AiService.GetUsage`).
    Status,
    /// Force a fresh deep-pass (re)analysis of one message, streaming
    /// progress as it arrives (`AiService.AnalyzeMessage`).
    Process {
        /// Message id to analyze.
        message_id: i64,
    },
    /// Print a message's cached AI summary (`AiService.GetSummary`) —
    /// never triggers a model call.
    Summary {
        /// Message id.
        message_id: i64,
        /// Print the raw structured result instead of a formatted view.
        #[arg(long)]
        json: bool,
    },
    /// Print a message's suggested reply, generating one now if none is
    /// cached yet (`AiService.SuggestReply`).
    Reply {
        /// Message id.
        message_id: i64,
    },
    /// Requeue quarantined AI jobs.
    Retry {
        /// Requeue every job that exhausted its retries
        /// (`AiService.RetryFailed`, `AiQueue::revive_all_dead`) — the only
        /// retry mode this build supports.
        #[arg(long)]
        failed: bool,
    },
    /// Pause the daemon's AI dispatch loop (`AiService.SetPaused`). Cached
    /// results stay readable; nothing new is enqueued or dispatched.
    Pause,
    /// Resume the daemon's AI dispatch loop (`AiService.SetPaused`).
    Resume,
    /// Token/cost usage against the configured caps (`AiService.GetUsage`).
    Cost {
        /// Show this calendar month's rollup instead of today's.
        #[arg(long)]
        month: bool,
    },
    /// Per-account and global spend budgets
    /// (`AiPolicyService.SetBudget`/`GetSpend`).
    Budget {
        #[command(subcommand)]
        action: BudgetAction,
    },
    /// Which inference backend an account's AI calls use
    /// (`AiPolicyService.SetAiProvider`/`GetAiProvider`).
    Provider {
        #[command(subcommand)]
        action: ProviderAction,
    },
    /// Scan one message for prompt-injection signals, exactly as the AI
    /// pipeline sees it (`AiSafetyService.ScanInjection`). Makes no model
    /// call and costs nothing.
    ///
    /// A rule that matched on a `claude_is` verdict will not fire its
    /// actions on a message flagged at or above
    /// `ai.injection.block_actions_at` until a human confirms it — that is
    /// what `--confirm` is for. Read the excerpts before you do: confirming
    /// says "I have looked at what this message tried and I still want the
    /// rule to act on it".
    #[command(name = "scan-injection")]
    ScanInjection {
        /// Message id.
        message_id: i64,
        /// Confirm the reported findings, releasing any withheld rule
        /// actions on this message (`AiSafetyService.ConfirmInjection`).
        #[arg(long)]
        confirm: bool,
        /// Withdraw a confirmation given earlier, so the shield withholds
        /// again.
        #[arg(long, conflicts_with = "confirm")]
        revoke: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderAction {
    /// Route one account's AI calls to a backend
    /// (`AiPolicyService.SetAiProvider`).
    ///
    /// `local` is always honored. `claude` only permits the hosted backend
    /// where `ai.policy` already allows it — a `local_only` folder stays
    /// on-device regardless, and a `forbidden` one is not processed at all.
    /// `inherit` clears the override so the account follows the daemon-wide
    /// setting again.
    Set {
        /// Account id from `mail account list`, or 0 for every account with
        /// no override of its own.
        account: i64,
        /// `local`, `claude`, or `inherit` to clear the override.
        backend: BackendArg,
    },
    /// Which backend an account uses, and whether the local model is ready
    /// (`AiPolicyService.GetAiProvider`).
    Status {
        /// Account id, or 0 for the daemon-wide scope.
        #[arg(default_value_t = 0)]
        account: i64,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum BackendArg {
    /// Fully on-device inference. Nothing leaves the machine.
    Local,
    /// The hosted Claude Messages API.
    Claude,
    /// Clear the override and follow the daemon-wide `ai.provider`.
    Inherit,
}

#[derive(Debug, Subcommand)]
enum BudgetAction {
    /// Store the caps for one scope (`AiPolicyService.SetBudget`).
    ///
    /// A cap left off the command line is left *uncapped*, not set to zero:
    /// the enforcer's boundary is `>=`, so `--daily-hard-usd 0` forbids all
    /// spending while omitting it forbids none. Setting a budget replaces
    /// whatever was stored for that scope, so pass every cap you want in
    /// force, not just the one you are changing.
    Set {
        /// Account id to budget, or 0 (the default) for the global budget
        /// every call counts toward.
        #[arg(long, default_value_t = 0)]
        account: i64,
        /// Budget the bulk sub-budget (backlog work) instead of the
        /// everything budget. A bulk call is checked against both.
        #[arg(long)]
        bulk: bool,
        /// Downgrade the model (opus -> sonnet -> haiku) at or above this
        /// many dollars spent today.
        #[arg(long)]
        daily_soft_usd: Option<f64>,
        /// Block dispatch at or above this many dollars spent today.
        #[arg(long)]
        daily_hard_usd: Option<f64>,
        /// Downgrade the model at or above this many tokens spent today.
        #[arg(long)]
        daily_soft_tokens: Option<i64>,
        /// Block dispatch at or above this many tokens spent today.
        #[arg(long)]
        daily_hard_tokens: Option<i64>,
        /// Downgrade the model at or above this many dollars spent this
        /// calendar month.
        #[arg(long)]
        monthly_soft_usd: Option<f64>,
        /// Block dispatch at or above this many dollars spent this calendar
        /// month.
        #[arg(long)]
        monthly_hard_usd: Option<f64>,
        /// Downgrade the model at or above this many tokens spent this
        /// calendar month.
        #[arg(long)]
        monthly_soft_tokens: Option<i64>,
        /// Block dispatch at or above this many tokens spent this calendar
        /// month.
        #[arg(long)]
        monthly_hard_tokens: Option<i64>,
    },
    /// Spend so far today and this month against the caps in force
    /// (`AiPolicyService.GetSpend`).
    Status {
        /// Account id to report, or 0 (the default) for the global budget.
        #[arg(long, default_value_t = 0)]
        account: i64,
    },
}

#[derive(Debug, clap::Args)]
struct ListArgs {
    /// Mailbox to list, by id.
    #[arg(long, conflicts_with = "all")]
    mailbox: Option<i64>,
    /// Merge every account's inbox into one time-ordered, deduplicated view.
    ///
    /// The same message delivered to two of your accounts appears once, and
    /// each row still names the account and folder it really lives in — which
    /// is what lets you act on it.
    #[arg(long)]
    all: bool,
    /// Rows per page (the server caps this at 500).
    #[arg(long, default_value_t = 50)]
    limit: i32,
    /// Continue from the token the previous page printed.
    #[arg(long, value_name = "TOKEN")]
    page_token: Option<String>,
}

#[derive(Debug, Subcommand)]
enum TagRuleAction {
    /// List an account's tag rules, enabled or not.
    List {
        #[arg(long, default_value_t = 1)]
        account: i64,
    },
    /// Create a rule, or re-point an existing one of the same name.
    Set {
        /// Rule name, unique per account. Re-using it re-points the rule
        /// rather than adding a second one beside it.
        name: String,
        /// The tag this rule governs, created on demand.
        tag: String,
        /// `suggest` (default) leaves every suggestion pending; `auto` lets
        /// one at or above the floor apply itself.
        #[arg(long, default_value = "suggest")]
        mode: String,
        /// This rule's confidence floor, 0.0..=1.0. It never lowers the
        /// global `tags.ai.auto_apply_min_confidence` — the effective floor
        /// is the higher of the two.
        #[arg(long, default_value_t = 0.9)]
        min_conf: f64,
        /// Retire the rule without deleting it.
        #[arg(long)]
        disabled: bool,
        #[arg(long, default_value_t = 1)]
        account: i64,
    },
}

#[derive(Debug, Subcommand)]
enum AccountAction {
    /// Discover an address's IMAP/SMTP settings and print a ready TOML block
    /// (`AccountService.Autoconfigure`).
    ///
    /// Probes the domain's autoconfig document, Mozilla's ISPDB, Microsoft
    /// autodiscover and RFC 6186 SRV records, validates whatever comes back,
    /// and — with a credential — verifies it by logging in. Nothing is
    /// written: the block is printed for you to paste into rmail.toml.
    Add {
        /// The email address to configure.
        email: String,
        /// A command whose stdout is the password. Supplying a credential is
        /// what lets the discovery be verified by a real login.
        #[arg(long, value_name = "COMMAND")]
        password_command: Option<String>,
        /// The name of an environment variable holding the password.
        #[arg(long, value_name = "VAR", conflicts_with = "password_command")]
        password_env: Option<String>,
        /// A macOS Keychain service name holding the password.
        #[arg(
            long,
            value_name = "SERVICE",
            conflicts_with_all = ["password_command", "password_env"]
        )]
        keychain: Option<String>,
        /// If every probe misses, let Claude propose settings from the
        /// domain, its MX records and the probe responses. Costs money, and
        /// the answer is a guess — it is validated and (with a credential)
        /// login-checked before you see it, but it is still a guess.
        #[arg(long)]
        ai: bool,
    },
    /// Authorize an account with OAuth2, opening a browser for consent.
    ///
    /// Runs the whole loopback+PKCE flow: the daemon binds a redirect port,
    /// prints the authorization URL, and waits for the browser to come back.
    /// The refresh token is written to the macOS Keychain by the daemon and
    /// never crosses this process.
    Login {
        /// Account id (as returned by `AccountService.List`).
        id: i64,
        /// Provider: google/gmail or microsoft/outlook.
        #[arg(long = "oauth", value_name = "PROVIDER")]
        oauth: String,
        /// OAuth client id of a desktop/native application you registered
        /// with the provider. Not a secret.
        #[arg(long)]
        client_id: String,
        /// A command whose stdout is the client secret, for providers that
        /// require one from a native client (Google's "Desktop app" type).
        /// The secret is never passed on the command line.
        #[arg(long, value_name = "COMMAND")]
        client_secret_command: Option<String>,
        /// Scope(s) to request. Omit for the provider's mail-only defaults.
        #[arg(long = "scope", value_delimiter = ',')]
        scopes: Vec<String>,
        /// Print the URL instead of trying to open a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Refresh an account's OAuth access token.
    Refresh {
        /// Account id.
        id: i64,
        /// Refresh even if the stored token has not expired yet.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TokenAction {
    /// Mint a new capability token. The bearer secret is printed exactly
    /// once — it cannot be recovered later, only revoked.
    Create {
        /// Human-readable label (e.g. "ci", "claude-agent").
        #[arg(long)]
        name: String,
        /// Scope(s) to grant: mail.read, mail.write, mail.send, ai.invoke,
        /// ai.spend:<usd>, mailbox:<name>, automation, admin. Repeatable
        /// and/or comma-separated, e.g. `--scope mail.read --scope
        /// ai.invoke` or `--scope mail.read,ai.invoke`. NOTE: ai.spend and
        /// mailbox are accepted and stored but not yet enforced by any RPC —
        /// a mailbox-only token grants nothing today, it does not restrict.
        #[arg(long = "scope", required = true, value_delimiter = ',')]
        scopes: Vec<String>,
        /// Time-to-live, e.g. "24h", "90d". Omit for no expiry.
        #[arg(long)]
        ttl: Option<String>,
    },
    /// List tokens (metadata only — never the secret or its hash).
    List,
    /// Revoke a token by id.
    Revoke {
        /// Token id (as printed by `mail token create`/`list`).
        id: i64,
    },
}

/// Deadline for the health-check RPC so a wedged daemon cannot hang the CLI.
const HEALTH_RPC_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(socket_path_from_env);

    match cli.command {
        Command::Ping => ping(&socket).await,
        Command::Sync {
            account,
            mailbox,
            full,
            watch,
        } => sync(&socket, account, mailbox, full, watch).await,
        Command::List(args) => list(&socket, args).await,
        Command::Account { action } => match action {
            AccountAction::Add {
                email,
                password_command,
                password_env,
                keychain,
                ai,
            } => account_add(&socket, email, password_command, password_env, keychain, ai).await,
            AccountAction::Login {
                id,
                oauth,
                client_id,
                client_secret_command,
                scopes,
                no_browser,
            } => {
                account_login(
                    &socket,
                    id,
                    oauth,
                    client_id,
                    client_secret_command,
                    scopes,
                    no_browser,
                )
                .await
            }
            AccountAction::Refresh { id, force } => account_refresh(&socket, id, force).await,
        },
        Command::Token { action } => match action {
            TokenAction::Create { name, scopes, ttl } => {
                token_create(&socket, name, scopes, ttl).await
            }
            TokenAction::List => token_list(&socket).await,
            TokenAction::Revoke { id } => token_revoke(&socket, id).await,
        },
        Command::Tui(args) => tui::run(&socket, args).await,
        Command::Keys { action } => keys_cli::run(action),
        Command::Search(args) => search_cli::search(&socket, args).await,
        Command::Folder { action } => folder_cli::dispatch(&socket, action).await,
        Command::Find(args) => find_cli::find(&socket, args).await,
        Command::Similar(args) => search_cli::similar(&socket, args).await,
        Command::Ai { action } => match action {
            AiAction::Status => ai_status(&socket).await,
            AiAction::Process { message_id } => ai_process(&socket, message_id).await,
            AiAction::Summary { message_id, json } => ai_summary(&socket, message_id, json).await,
            AiAction::Reply { message_id } => ai_reply(&socket, message_id).await,
            AiAction::Retry { failed } => ai_retry(&socket, failed).await,
            AiAction::Pause => ai_set_paused(&socket, true).await,
            AiAction::Resume => ai_set_paused(&socket, false).await,
            AiAction::Cost { month } => ai_cost(&socket, month).await,
            AiAction::Budget { action } => ai_budget(&socket, action).await,
            AiAction::Provider { action } => ai_provider(&socket, action).await,
            AiAction::ScanInjection {
                message_id,
                confirm,
                revoke,
            } => ai_scan_injection(&socket, message_id, confirm, revoke).await,
        },
        Command::Ask(args) => ask(&socket, args).await,
        Command::Note { action } => note_cli::dispatch(&socket, action).await,
        Command::Notes(args) => note_cli::list(&socket, args).await,
        Command::Export(args) => export_cli::export(&socket, args).await,
        Command::Attach { action } => match action {
            extract_cli::AttachAction::Tables(args) => extract_cli::tables(&socket, args).await,
            extract_cli::AttachAction::Invoice(args) => extract_cli::invoice(&socket, args).await,
        },
        Command::Invoices(args) => extract_cli::invoices(&socket, args).await,
        Command::Extract { action } => match action {
            extract_cli::ExtractAction::Events(args) => extract_cli::events(&socket, args).await,
            extract_cli::ExtractAction::Tasks(args) => extract_cli::tasks(&socket, args).await,
            extract_cli::ExtractAction::Data(args) => extract_cli::structured(&socket, args).await,
        },
        Command::Links(args) => extract_cli::links(&socket, args).await,
        Command::Hook { action } => hook_cli::run(&socket, action).await,
        Command::Webhook { action } => webhook_cli::run(&socket, action).await,
        Command::Forward(args) => webhook_cli::forward(&socket, args).await,
        Command::Notify { action } => notify_cli::run(&socket, action).await,
        Command::Index { action } => index_cli::run(&socket, action).await,
        Command::Entities(args) => index_cli::entities(&socket, args).await,
        Command::Tag(args) => tag_cli::tag(&socket, args).await,
        Command::TagBulk {
            query,
            account,
            tags,
        } => tag_cli::bulk_tag(&socket, account, query, tags).await,
        Command::Untag(args) => tag_cli::untag(&socket, args).await,
        Command::Tags(args) => tag_cli::tags(&socket, args).await,
        Command::SuggestTags { message_id } => tag_cli::suggest_tags(&socket, message_id).await,
        Command::Reply(args) => reply_cli::reply(&socket, args).await,
        Command::Draft { action } => reply_cli::dispatch(&socket, action).await,
        Command::Send(args) => outbox_cli::send(&socket, args).await,
        Command::Undo(args) => outbox_cli::undo(&socket, args).await,
        Command::Outbox(args) => outbox_cli::outbox(&socket, args).await,
        Command::Followup { action } => outbox_cli::followup(&socket, action).await,
        Command::Stats { action } => stats_cli::run(&socket, action).await,
        Command::Contact(args) => analytics_cli::contact(&socket, args).await,
        Command::Subs(args) => analytics_cli::subs(&socket, args).await,
        Command::Digest(args) => digest_cli::run(&socket, args).await,
        Command::Mcp { action } => mcp_cli::run(&socket, action).await,
        Command::AcceptTags { message_tag_ids } => {
            tag_cli::resolve_suggestions(&socket, message_tag_ids, true).await
        }
        Command::RejectTags { message_tag_ids } => {
            tag_cli::resolve_suggestions(&socket, message_tag_ids, false).await
        }
        Command::TagRules { action } => match action {
            TagRuleAction::List { account } => tag_cli::list_tag_rules(&socket, account).await,
            TagRuleAction::Set {
                name,
                tag,
                mode,
                min_conf,
                disabled,
                account,
            } => {
                tag_cli::set_tag_rule(&socket, account, &name, &tag, &mode, min_conf, !disabled)
                    .await
            }
        },
    }
}

/// Trigger a sync pass and, with `--watch`, keep streaming what changes after
/// it.
async fn sync(
    socket: &Path,
    account_id: i64,
    mailbox_id: Option<i64>,
    full: bool,
    watch: bool,
) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = SyncServiceClient::new(channel);

    let response = client
        .sync_folder(SyncFolderRequest {
            account_id,
            mailbox_id,
            mode: if full {
                SyncMode::Full as i32
            } else {
                SyncMode::Auto as i32
            },
        })
        .await
        .context("sync RPC failed")?
        .into_inner();

    let mut failures = 0;
    for folder in &response.folders {
        match &folder.error {
            Some(error) => {
                failures += 1;
                println!("{:<24} failed: {error}", folder.mailbox_name);
            }
            None => println!(
                "{:<24} {:<10} +{} new  ~{} flags  -{} gone",
                folder.mailbox_name,
                folder.strategy,
                folder.new_messages,
                folder.flag_updates,
                folder.expunged
            ),
        }
    }
    if response.folders.is_empty() {
        println!("no folders to sync");
    }

    if watch {
        // Resume from where this pass ended, so the stream shows what happens
        // *next* rather than replaying what was just reported.
        println!("watching from seq {}…", response.latest_seq);
        watch_events(&mut client, account_id, response.latest_seq).await?;
    }

    if failures > 0 {
        bail!("{failures} folder(s) failed to sync");
    }
    Ok(())
}

/// Follow the event stream until the daemon closes it or the user interrupts.
async fn watch_events(
    client: &mut SyncServiceClient<tonic::transport::Channel>,
    account_id: i64,
    since_seq: i64,
) -> Result<()> {
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq,
            kinds: Vec::new(),
        })
        .await
        .context("watch RPC failed")?
        .into_inner();

    loop {
        // Ctrl-C ends the watch cleanly rather than killing the process
        // mid-write, so a terminal is never left with a half-printed line.
        let next = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!();
                return Ok(());
            }
            next = stream.next() => next,
        };
        match next {
            Some(Ok(event)) => {
                let kind = EventKind::try_from(event.kind)
                    .map(|k| k.as_str_name().trim_start_matches("EVENT_KIND_").to_owned())
                    .unwrap_or_else(|_| format!("KIND_{}", event.kind));
                println!(
                    "seq {:<8} {:<14} {}",
                    event.seq,
                    kind,
                    event.payload.trim_matches('"')
                );
            }
            Some(Err(status)) => {
                // A retention gap is the one stream error a client is expected
                // to act on, and the daemon reports where to resume in
                // structured metadata rather than in the message text.
                bail!(
                    "event stream ended: {} ({})",
                    status.message(),
                    status.code()
                );
            }
            None => return Ok(()),
        }
    }
}

async fn ping(socket: &Path) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;

    let mut client = HealthClient::new(channel);
    let response = tokio::time::timeout(
        HEALTH_RPC_TIMEOUT,
        client.check(HealthCheckRequest {
            service: String::new(),
        }),
    )
    .await
    .context("health check RPC timed out")?
    .context("health check RPC failed")?;

    let status = response.into_inner().status();
    println!("rmaild health: {status:?}");

    if status == ServingStatus::Serving {
        Ok(())
    } else {
        bail!("rmaild is not serving (status: {status:?})");
    }
}

/// Mint a capability token and print its bearer secret. This is the only
/// moment the secret is ever visible — `ListTokens` returns metadata only.
/// `mail account login --oauth <provider>`: the whole authorization, driven
/// from one verb.
///
/// Two RPCs rather than one, because the middle of this flow is a human in a
/// browser: `BeginOAuth` returns the URL and binds the port, and
/// `CompleteOAuth` blocks until the redirect lands. Splitting them is what
/// lets the URL be printed *before* the wait starts — a single RPC would have
/// to stream, and would still leave the caller with no way to see the URL if
/// opening a browser failed.
async fn account_login(
    socket: &Path,
    id: i64,
    provider: String,
    client_id: String,
    client_secret_command: Option<String>,
    scopes: Vec<String>,
    no_browser: bool,
) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AccountServiceClient::new(channel);

    let begun = client
        .begin_o_auth(BeginOAuthRequest {
            account_id: id,
            provider: provider.clone(),
            client_id,
            client_secret_command,
            scopes,
        })
        .await
        .context("BeginOAuth RPC failed")?
        .into_inner();

    println!("Open this URL to authorize rmail for account {id}:");
    println!();
    println!("  {}", begun.authorization_url);
    println!();
    if !no_browser {
        open_in_browser(&begun.authorization_url).await;
    }
    println!("Waiting for the redirect to {} …", begun.redirect_uri);

    // No client-side deadline: the daemon already bounds this flow, and a
    // shorter one here would abandon a user who is still typing a password.
    let done = client
        .complete_o_auth(CompleteOAuthRequest {
            flow_id: begun.flow_id,
        })
        .await
        .context("CompleteOAuth RPC failed")?
        .into_inner();

    println!();
    println!("Authorized with {}.", done.provider);
    println!("scopes:  {}", done.scopes.join(" "));
    println!("expires: {} (unix seconds)", done.expires_at);
    println!("The refresh token is in your Keychain; it never passed through this command.");
    Ok(())
}

/// `mail list`: one mailbox, or every account's inbox merged.
///
/// The next page's token comes back in the call's *initial metadata* rather
/// than in the body — a server-streamed response has no envelope to carry one
/// (see `rmail_core::page`) — so it is read off the response before the frames
/// are drained, and printed for the caller to pass back with `--page-token`.
/// Its absence is definitive: that was the last page.
async fn list(socket: &Path, args: ListArgs) -> Result<()> {
    use rmail_proto::v1::mail_service_client::MailServiceClient;
    use rmail_proto::v1::{ListMessagesRequest, ListUnifiedRequest};

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = MailServiceClient::new(channel);
    let page_token = args.page_token.unwrap_or_default();

    let response = if args.all {
        client
            .list_unified(ListUnifiedRequest {
                page_size: args.limit,
                page_token,
            })
            .await
            .context("ListUnified RPC failed")?
    } else {
        let mailbox_id = args
            .mailbox
            .context("`mail list` needs --mailbox <id>, or --all for every account's inbox")?;
        client
            .list(ListMessagesRequest {
                mailbox_id,
                page_size: args.limit,
                page_token,
            })
            .await
            .context("List RPC failed")?
    };

    let next = response
        .metadata()
        .get(rmail_core::page::NEXT_PAGE_TOKEN_METADATA_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let mut stream = response.into_inner();
    let mut shown = 0usize;
    while let Some(message) = stream.message().await.context("List stream failed")? {
        shown += 1;
        // The account and mailbox are printed even for a single-mailbox
        // listing: in the unified view they are the answer to "where does
        // this actually live", and a format that changed between the two
        // would be worse than one that is occasionally redundant.
        println!(
            "{:>8}  acct {:<3} mbox {:<3} {:<20} {:<28} {}",
            message.id,
            message.account_id,
            message.mailbox_id,
            message
                .date
                .and_then(|date| chrono::DateTime::<chrono::Utc>::from_timestamp(date, 0))
                .map_or_else(
                    || "-".to_owned(),
                    |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                ),
            cell(message.from_addr.as_deref().unwrap_or("-"), 28),
            cell(message.subject.as_deref().unwrap_or("(no subject)"), 60),
        );
    }
    match next {
        Some(token) => println!("\n{shown} shown; next page: --page-token {token}"),
        None => println!("\n{shown} shown; end of list"),
    }
    Ok(())
}

/// `mail account add <email>`: discover, validate, verify, print.
async fn account_add(
    socket: &Path,
    email: String,
    password_command: Option<String>,
    password_env: Option<String>,
    keychain: Option<String>,
    ai: bool,
) -> Result<()> {
    use rmail_proto::v1::{credential_ref, AutoconfigureRequest, CredentialRef};

    let credential = password_command
        .map(credential_ref::Source::PasswordCommand)
        .or_else(|| password_env.map(credential_ref::Source::PasswordEnv))
        .or_else(|| keychain.map(credential_ref::Source::Keychain))
        .map(|source| CredentialRef {
            source: Some(source),
        });

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let response = AccountServiceClient::new(channel)
        .autoconfigure(AutoconfigureRequest {
            email,
            credential,
            allow_model_fallback: ai,
        })
        .await
        .context("Autoconfigure RPC failed")?
        .into_inner();

    // Everything printed here crossed a wire and most of it originated
    // further out still — a hostname from someone's autoconfig document, a
    // refusal quoted from a remote IMAP server. It is sanitized on the way to
    // the terminal for the same reason a subject line is.
    if let Some(imap) = &response.imap {
        println!(
            "imap: {}:{} ({}) as {}   [source: {}]",
            sanitized(&imap.host),
            imap.port,
            sanitized(&imap.security),
            sanitized(&imap.username),
            sanitized(&response.source)
        );
    }
    match &response.smtp {
        Some(smtp) => println!(
            "smtp: {}:{} ({})",
            sanitized(&smtp.host),
            smtp.port,
            sanitized(&smtp.security)
        ),
        None => println!("smtp: not discovered"),
    }
    if response.login_validated {
        println!("login: verified");
    } else {
        println!("login: {}", sanitized(&response.validation_detail));
    }
    for warning in &response.warnings {
        println!("warning: {}", sanitized(warning));
    }
    println!();
    // Not sanitized, and deliberately: this is a configuration file fragment
    // whose newlines are its structure. Its values are TOML-escaped by the
    // serializer that produced it (see `autoconfig::render_toml`), which is
    // the encoding that matters for text destined for a file.
    print!("{}", response.toml);
    Ok(())
}

/// One column of a listing row: sanitized, then truncated.
///
/// A subject and a From line are attacker-controlled text on their way to a
/// terminal, so the control characters go first — an unsanitized subject can
/// carry an escape sequence that repaints the screen, and truncation would
/// happily cut one in half and leave the terminal in that state. Dropping
/// them (rather than substituting a placeholder) matches what `mail search`
/// already does; see `search_cli`'s "Terminal safety" section.
fn cell(value: &str, width: usize) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in sanitized(value).chars() {
        out.push(ch);
        if out.chars().count() >= width {
            // One character of headroom is spent on the ellipsis, so a
            // truncated cell is visibly truncated.
            if value.chars().count() > width {
                out.pop();
                out.push('…');
            }
            break;
        }
    }
    out
}

/// Text from somewhere else, made safe to print.
///
/// Used for every line this file prints that did not originate in it: a
/// listing's subject and sender, and `mail account add`'s warnings and
/// validation detail — the latter is `format!("login failed: {error}")`
/// wrapping whatever a remote IMAP server said, which is exactly the shape of
/// input this exists for. Whitespace is folded to spaces and other control
/// characters are dropped, matching `mail search` (see `search_cli`'s
/// "Terminal safety" section).
fn sanitized(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\n' | '\r' | '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

async fn account_refresh(socket: &Path, id: i64, force: bool) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let response = AccountServiceClient::new(channel)
        .refresh_token(RefreshTokenRequest {
            account_id: id,
            force,
        })
        .await
        .context("RefreshToken RPC failed")?
        .into_inner();

    println!(
        "{} ({}): expires {} (unix seconds)",
        if response.refreshed {
            "refreshed"
        } else {
            "still valid"
        },
        response.provider,
        response.expires_at
    );
    Ok(())
}

/// Best-effort browser launch. A failure is not an error: the URL has already
/// been printed, and the flow works perfectly well with a copy and paste.
///
/// Reaped rather than detached. `open`/`xdg-open` hand off and exit at once,
/// but the very next thing this command does is block for up to five minutes
/// waiting for the redirect — long enough for an unreaped child to sit there
/// as a zombie for the whole flow.
async fn open_in_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    // `arg`, not a shell: the URL is provider-controlled data and must never
    // be word-split or interpreted by `sh`.
    let spawned = tokio::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status();
    // Bounded: a wedged opener must not hold up the flow whose URL is already
    // on screen.
    match tokio::time::timeout(Duration::from_secs(10), spawned).await {
        Ok(Ok(_)) => println!("(opening it in your browser…)"),
        _ => println!("(could not launch a browser; open the URL above by hand)"),
    }
}

async fn token_create(
    socket: &Path,
    name: String,
    scopes: Vec<String>,
    ttl: Option<String>,
) -> Result<()> {
    let ttl_secs = ttl
        .as_deref()
        .map(|s| {
            rmail_core::config::parse_human_duration(s)
                .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        })
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid --ttl: {e}"))?;

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AdminServiceClient::new(channel);
    let response = client
        .mint_token(MintTokenRequest {
            name,
            scopes,
            ttl_secs,
        })
        .await
        .context("MintToken RPC failed")?
        .into_inner();

    println!("id:      {}", response.id);
    println!("name:    {}", response.name);
    println!("scopes:  {}", response.scopes.join(","));
    if let Some(expires_at) = response.expires_at {
        println!("expires: {expires_at} (unix seconds)");
    } else {
        println!("expires: never");
    }
    println!();
    println!("token:   {}", response.token);
    println!();
    println!(
        "Store this now — it cannot be shown again. Revoke with `mail token revoke {}`.",
        response.id
    );
    Ok(())
}

/// List tokens (metadata only).
async fn token_list(socket: &Path) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AdminServiceClient::new(channel);
    let response = client
        .list_tokens(ListTokensRequest {})
        .await
        .context("ListTokens RPC failed")?
        .into_inner();

    if response.tokens.is_empty() {
        println!("no tokens");
        return Ok(());
    }
    for token in response.tokens {
        let status = if token.revoked { "revoked" } else { "active" };
        println!(
            "{:<6} {:<20} {:<8} {}",
            token.id,
            token.name,
            status,
            token.scopes.join(",")
        );
    }
    Ok(())
}

/// Revoke a token by id.
async fn token_revoke(socket: &Path, id: i64) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AdminServiceClient::new(channel);
    client
        .revoke_token(RevokeTokenRequest { id })
        .await
        .context("RevokeToken RPC failed")?;
    println!("revoked token {id}");
    Ok(())
}

// ---------------------------------------------------------------------------
// `mail ai ...`
// ---------------------------------------------------------------------------

async fn ai_client(socket: &Path) -> Result<AiServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(AiServiceClient::new(channel))
}

/// Queue depth, today's tokens/cost, headroom, and pause state.
async fn ai_status(socket: &Path) -> Result<()> {
    let usage = ai_client(socket)
        .await?
        .get_usage(GetUsageRequest {})
        .await
        .context("GetUsage RPC failed")?
        .into_inner();

    println!("enabled: {}", usage.enabled);
    println!("paused:  {}", usage.paused);
    if let Some(queue) = &usage.queue {
        println!(
            "queue:   {} ready, {} backing off, {} leased, {} dead",
            queue.ready, queue.backing_off, queue.leased, queue.dead
        );
    }
    if let Some(today) = &usage.today {
        println!(
            "today:   {} request(s), {} tokens, ${:.4}",
            today.requests,
            today.input_tokens + today.output_tokens,
            today.cost_usd
        );
    }
    println!(
        "caps:    ${:.2}/day, ${:.2}/month, {} tokens/day",
        usage.daily_cost_cap_usd, usage.monthly_cost_cap_usd, usage.daily_token_cap
    );
    Ok(())
}

/// Force a fresh deep-pass analysis, printing tokens as they stream in and
/// the final structured result once the daemon has persisted it.
async fn ai_process(socket: &Path, message_id: i64) -> Result<()> {
    let mut stream = ai_client(socket)
        .await?
        .analyze_message(AnalyzeMessageRequest { message_id })
        .await
        .context("AnalyzeMessage RPC failed")?
        .into_inner();

    let mut printed_any_token = false;
    while let Some(event) = stream.next().await {
        let event = event.context("analyze stream ended with an error")?;
        match event.event {
            Some(analyze_event::Event::Token(token)) => {
                print!("{token}");
                printed_any_token = true;
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            Some(analyze_event::Event::ToolUseStart(tool)) => {
                println!("\n[tool use: {}]", tool.name);
            }
            // Streamed live as it arrives; the durable count that matters
            // (and is billed) lives in the audit ledger, not this echo.
            Some(analyze_event::Event::Usage(_)) => {}
            Some(analyze_event::Event::Done(done)) => {
                if printed_any_token {
                    println!();
                }
                println!("stop_reason: {}", done.stop_reason);
                if let Some(summary) = done.result {
                    println!();
                    print_summary(&summary);
                }
            }
            None => {}
        }
    }
    Ok(())
}

/// Ask the mailbox a question and stream the grounded answer.
///
/// Tokens stream to stdout as they arrive; citations arrive after the prose
/// (they are only resolvable once the whole answer has been seen — see
/// `rmail_core::ai::rag`) and are printed as a numbered source list matching
/// the `[n]` markers already in the text.
///
/// An ungrounded answer is called out explicitly rather than printed as if it
/// were sourced: `AskDone.grounded` is the daemon's verdict on whether the
/// answer cited anything real, and silently dropping it would be presenting an
/// uncited answer as a cited one.
async fn ask(socket: &Path, args: AskArgs) -> Result<()> {
    let mut stream = ai_client(socket)
        .await?
        .ask_mailbox(AskRequest {
            question: args.question,
            account_id: args.account.unwrap_or(0),
            filter: args.filter,
            top_k: args.top_k.unwrap_or(0),
        })
        .await
        .context("AskMailbox RPC failed")?
        .into_inner();

    let mut citations: Vec<Citation> = Vec::new();
    let mut printed_any_token = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("ask stream ended with an error")?;
        match chunk.body {
            Some(ask_chunk::Body::Trace(trace)) => {
                if args.trace {
                    println!(
                        "retrieved {} · packed {} · withheld by policy {} · dropped for budget \
                         {} · ~{} context tokens{}",
                        trace.retrieved,
                        trace.packed,
                        trace.withheld_by_policy,
                        trace.dropped_for_budget,
                        trace.context_tokens,
                        if trace.model.is_empty() {
                            String::new()
                        } else {
                            format!(" · {}", trace.model)
                        }
                    );
                    println!();
                }
            }
            Some(ask_chunk::Body::Token(token)) => {
                print!("{}", terminal_safe(&token));
                printed_any_token = true;
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            Some(ask_chunk::Body::Citation(citation)) => citations.push(citation),
            // Streamed live as it arrives; the durable count that matters
            // (and is billed) lives in the audit ledger, not this echo.
            Some(ask_chunk::Body::Usage(_)) => {}
            Some(ask_chunk::Body::Done(done)) => {
                if printed_any_token {
                    println!();
                }
                if !citations.is_empty() {
                    println!("\nSources:");
                    for citation in &citations {
                        print_citation(citation);
                    }
                }
                if !done.grounded {
                    println!("\nnot grounded: {}", done.refusal);
                }
            }
            None => {}
        }
    }
    Ok(())
}

/// Make model- and mail-authored text safe to write to a terminal.
///
/// `mail ask` streams two kinds of attacker-influenced text straight to
/// stdout: the model's prose (steerable by any message that reached the
/// context) and each citation's subject, sender and quote (written by whoever
/// sent the mail). Both need neutralizing, and they need *different*
/// neutralizing than `search_cli`'s renderer does — that one folds newlines to
/// spaces, which is right for a one-line hit and wrong for a prose answer
/// where paragraph breaks carry meaning.
///
/// Two families, because they fail differently:
///
/// - **Bidi overrides and invisibles** reorder or hide what the user reads
///   without corrupting anything. `injection::sanitize_model_text` already
///   removes these and is what `rules::classify` applies to a `claude_is`
///   explanation; reusing it keeps one definition of "safe to show".
/// - **C0/C1 controls** are what actually drives a terminal — an `ESC [` run
///   can repaint the screen, move the cursor, or hide subsequent output.
///   Dropped here rather than escaped, matching `search_cli::push_sanitized`'s
///   reasoning: a visible placeholder buys nothing when the text is prose.
///
/// `\n` survives; `\t` becomes a space so it cannot be used for alignment
/// tricks in the citation block.
///
/// The TUI's overlays (task 85) draw the same two families of text onto a
/// screen an attacker would very much like to repaint, and go through this
/// same function — see `tui::overlays::safe_line`, which only adds the
/// newline folding a one-line table row needs.
pub(crate) fn terminal_safe(text: &str) -> String {
    text.chars().filter_map(terminal_safe_char).collect()
}

/// [`terminal_safe`] for one character, and the definition the whole-string
/// form is built from.
///
/// `None` means the character is dropped. Exposed per-character because the
/// TUI's highlighter cannot use the string form: it decides highlighting from
/// each character's position in the *original* text and only then emits the
/// safe form, and building a one-character `String` per glyph on every frame
/// of a streaming search is a lot of allocation for nothing.
pub(crate) fn terminal_safe_char(ch: char) -> Option<char> {
    if !rmail_core::ai::injection::is_display_safe(ch) {
        return None;
    }
    match ch {
        '\n' => Some('\n'),
        '\t' => Some(' '),
        c if c.is_control() => None,
        c => Some(c),
    }
}

fn print_citation(citation: &Citation) {
    let subject = if citation.subject.is_empty() {
        "(no subject)".to_owned()
    } else {
        terminal_safe(&citation.subject)
    };
    println!(
        "  [{}] #{} {} — {} ({}, uid {})",
        citation.label,
        citation.message_id,
        subject,
        terminal_safe(&citation.from_addr),
        terminal_safe(&citation.mailbox),
        citation.message_uid
    );
    if !citation.quote.is_empty() {
        println!("      {}", terminal_safe(&citation.quote));
    }
}

/// Print a message's cached AI summary. Never calls the model.
async fn ai_summary(socket: &Path, message_id: i64, json: bool) -> Result<()> {
    let summary = ai_client(socket)
        .await?
        .get_summary(GetSummaryRequest { message_id })
        .await
        .context("GetSummary RPC failed")?
        .into_inner();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary_to_json(&summary))?
        );
    } else {
        print_summary(&summary);
    }
    Ok(())
}

/// Print a message's suggested reply, generating one now (subject to
/// `ai.limits`' spend caps) if none is cached yet.
async fn ai_reply(socket: &Path, message_id: i64) -> Result<()> {
    let summary = ai_client(socket)
        .await?
        .suggest_reply(SuggestReplyRequest { message_id })
        .await
        .context("SuggestReply RPC failed")?
        .into_inner();

    match summary.suggested_reply {
        Some(reply) if !reply.trim().is_empty() => println!("{reply}"),
        _ => println!("no suggested reply for this message"),
    }
    Ok(())
}

/// Requeue every quarantined AI job.
async fn ai_retry(socket: &Path, failed: bool) -> Result<()> {
    if !failed {
        bail!("mail ai retry currently only supports `--failed` (requeue every dead job)");
    }
    let response = ai_client(socket)
        .await?
        .retry_failed(RetryFailedRequest {})
        .await
        .context("RetryFailed RPC failed")?
        .into_inner();
    println!("revived {} job(s)", response.revived);
    Ok(())
}

/// Pause or resume the daemon's AI dispatch loop.
async fn ai_set_paused(socket: &Path, paused: bool) -> Result<()> {
    let response = ai_client(socket)
        .await?
        .set_paused(SetPausedRequest { paused })
        .await
        .context("SetPaused RPC failed")?
        .into_inner();
    println!(
        "ai dispatch loop {}",
        if response.paused { "paused" } else { "resumed" }
    );
    Ok(())
}

/// Token/cost usage for today or, with `--month`, this calendar month.
async fn ai_cost(socket: &Path, month: bool) -> Result<()> {
    let usage = ai_client(socket)
        .await?
        .get_usage(GetUsageRequest {})
        .await
        .context("GetUsage RPC failed")?
        .into_inner();

    let Some(period) = (if month { usage.month } else { usage.today }) else {
        println!("no usage recorded");
        return Ok(());
    };
    println!(
        "{}: {} request(s), {} input tokens, {} output tokens, ${:.4}",
        period.day, period.requests, period.input_tokens, period.output_tokens, period.cost_usd
    );
    Ok(())
}

async fn ai_policy_client(
    socket: &Path,
) -> Result<AiPolicyServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(AiPolicyServiceClient::new(channel))
}

async fn ai_safety_client(
    socket: &Path,
) -> Result<AiSafetyServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(AiSafetyServiceClient::new(channel))
}

/// `mail ai scan-injection <id> [--confirm|--revoke]`.
///
/// Always scans first, even when confirming: a confirmation is consent to a
/// specific set of findings (the daemon clears it when a re-scan turns up
/// different ones), so confirming without having just seen them would be
/// consenting to whatever a stale row happened to hold.
async fn ai_scan_injection(
    socket: &Path,
    message_id: i64,
    confirm: bool,
    revoke: bool,
) -> Result<()> {
    let mut client = ai_safety_client(socket).await?;
    let scan = client
        .scan_injection(ScanInjectionRequest { message_id })
        .await
        .context("ScanInjection RPC failed")?
        .into_inner();
    print_injection_scan(&scan);

    if !confirm && !revoke {
        return Ok(());
    }
    if !scan.flagged {
        // Not an error: the user asked for a state this message is already
        // in. Saying so is more useful than a NOT_FOUND from the daemon.
        println!(
            "\nnothing to {}: this message is not flagged",
            if confirm { "confirm" } else { "revoke" }
        );
        return Ok(());
    }
    let flag = client
        .confirm_injection(ConfirmInjectionRequest {
            message_id,
            confirmed: confirm,
        })
        .await
        .context("ConfirmInjection RPC failed")?
        .into_inner()
        .flag;
    match flag {
        Some(flag) if flag.confirmed_at > 0 => {
            println!("\nconfirmed: AI-decided rule actions may now act on message {message_id}")
        }
        Some(_) => println!(
            "\nconfirmation withdrawn: AI-decided rule actions on message {message_id} are \
             withheld again"
        ),
        None => println!("\nthe daemon returned no flag"),
    }
    Ok(())
}

fn print_injection_scan(scan: &ScanInjectionResponse) {
    if !scan.flagged {
        println!("message {}: no prompt-injection signals", scan.message_id);
        return;
    }
    let severity = match InjectionSeverity::try_from(scan.severity) {
        Ok(InjectionSeverity::Hostile) => "hostile",
        Ok(InjectionSeverity::Suspicious) => "suspicious",
        _ => "unknown",
    };
    println!("message {}: FLAGGED ({severity})", scan.message_id);
    println!("kinds:   {}", scan.kinds.join(", "));
    println!(
        "actions: {}",
        if scan.actions_withheld {
            "WITHHELD — a rule matching on claude_is will not act on this message"
        } else if scan.confirmed_at > 0 {
            "allowed (confirmed)"
        } else {
            "allowed (below the configured block threshold)"
        }
    );
    println!("\nwhat it tried:");
    for detection in &scan.detections {
        println!("  [{}] {}", detection.kind, detection.excerpt);
    }
}

/// `mail ai budget set/status`.
async fn ai_budget(socket: &Path, action: BudgetAction) -> Result<()> {
    match action {
        BudgetAction::Set {
            account,
            bulk,
            daily_soft_usd,
            daily_hard_usd,
            daily_soft_tokens,
            daily_hard_tokens,
            monthly_soft_usd,
            monthly_hard_usd,
            monthly_soft_tokens,
            monthly_hard_tokens,
        } => {
            let class = if bulk {
                BudgetClass::Bulk
            } else {
                BudgetClass::All
            };
            let response = ai_policy_client(socket)
                .await?
                .set_budget(SetBudgetRequest {
                    account_id: account,
                    class: class.into(),
                    caps: Some(BudgetCaps {
                        daily: Some(BudgetWindowCaps {
                            soft_usd: daily_soft_usd,
                            hard_usd: daily_hard_usd,
                            soft_tokens: daily_soft_tokens,
                            hard_tokens: daily_hard_tokens,
                        }),
                        monthly: Some(BudgetWindowCaps {
                            soft_usd: monthly_soft_usd,
                            hard_usd: monthly_hard_usd,
                            soft_tokens: monthly_soft_tokens,
                            hard_tokens: monthly_hard_tokens,
                        }),
                    }),
                })
                .await
                .context("SetBudget RPC failed")?
                .into_inner();
            println!(
                "budget stored for {} ({})",
                scope_label(response.account_id),
                class_label(response.class())
            );
            if let Some(caps) = &response.caps {
                print_caps(caps);
            }
            Ok(())
        }
        BudgetAction::Status { account } => {
            let spend = ai_policy_client(socket)
                .await?
                .get_spend(GetSpendRequest {
                    account_id: account,
                })
                .await
                .context("GetSpend RPC failed")?
                .into_inner();
            println!(
                "{} — day {}, month {}",
                scope_label(spend.account_id),
                spend.day,
                spend.month
            );
            for class in [spend.all.as_ref(), spend.bulk.as_ref()]
                .into_iter()
                .flatten()
            {
                print_class_spend(class);
            }
            Ok(())
        }
    }
}

fn scope_label(account_id: i64) -> String {
    if account_id == 0 {
        "global budget".to_owned()
    } else {
        format!("account {account_id}")
    }
}

/// `mail ai provider set|status` — the operator surface for the local-only
/// model path (task 78).
async fn ai_provider(socket: &Path, action: ProviderAction) -> Result<()> {
    match action {
        ProviderAction::Set { account, backend } => {
            let response = ai_policy_client(socket)
                .await?
                .set_ai_provider(SetAiProviderRequest {
                    account_id: account,
                    provider: AiProviderKind::from(backend).into(),
                })
                .await
                .context("SetAiProvider RPC failed")?
                .into_inner();
            println!(
                "{}: override {} → calls now use {}",
                provider_scope_label(response.account_id),
                backend_label(response.provider()),
                backend_label(response.effective()),
            );
        }
        ProviderAction::Status { account } => {
            let response = ai_policy_client(socket)
                .await?
                .get_ai_provider(GetAiProviderRequest {
                    account_id: account,
                })
                .await
                .context("GetAiProvider RPC failed")?
                .into_inner();
            println!("{}", provider_scope_label(response.account_id));
            println!(
                "  configured (ai.provider): {}",
                backend_label(response.configured())
            );
            println!(
                "  override:                 {}",
                backend_label(response.account_override())
            );
            println!(
                "  effective:                {}",
                backend_label(response.effective())
            );
            println!("  ai.policy mode:           {}", response.policy_mode);
            // The structural half of the guarantee, reported so an operator
            // can confirm it from outside the daemon rather than taking the
            // config file's word for it.
            println!(
                "  network provider built:   {}",
                if response.network_provider_built {
                    "yes"
                } else {
                    "no (nothing in this daemon can dial out for AI)"
                }
            );
            println!("  local model:              {}", response.local_model);
            println!(
                "  local ready:              {}",
                if response.local_ready { "yes" } else { "no" }
            );
            // Printed whether or not the path is ready: when it is, this says
            // where the weights were found; when it is not, it is the fix.
            println!("  {}", response.local_detail);
        }
    }
    Ok(())
}

fn provider_scope_label(account_id: i64) -> String {
    if account_id == 0 {
        "daemon-wide (every account with no override of its own)".to_owned()
    } else {
        format!("account {account_id}")
    }
}

impl From<BackendArg> for AiProviderKind {
    fn from(value: BackendArg) -> Self {
        match value {
            BackendArg::Local => Self::Local,
            BackendArg::Claude => Self::Claude,
            BackendArg::Inherit => Self::Unspecified,
        }
    }
}

fn backend_label(kind: AiProviderKind) -> &'static str {
    match kind {
        AiProviderKind::Local => "local (on-device, zero egress)",
        AiProviderKind::Claude => "claude (hosted)",
        AiProviderKind::Unspecified => "none (inherits the daemon-wide setting)",
    }
}

fn class_label(class: BudgetClass) -> &'static str {
    match class {
        BudgetClass::All => "all",
        BudgetClass::Bulk => "bulk",
        BudgetClass::Unspecified => "unspecified",
    }
}

fn print_caps(caps: &BudgetCaps) {
    for (window, window_caps) in [
        ("daily", caps.daily.as_ref()),
        ("monthly", caps.monthly.as_ref()),
    ] {
        let Some(window_caps) = window_caps else {
            continue;
        };
        println!(
            "  {window:<8} soft ${} / hard ${}, soft {} / hard {} tokens",
            opt_usd(window_caps.soft_usd),
            opt_usd(window_caps.hard_usd),
            opt_count(window_caps.soft_tokens),
            opt_count(window_caps.hard_tokens),
        );
    }
}

fn print_class_spend(class: &ClassSpend) {
    let source = if class.stored {
        "set"
    } else {
        "derived from ai.limits"
    };
    println!("\n{} budget ({source}):", class_label(class.class()));
    for (window, spend) in [
        ("daily", class.daily.as_ref()),
        ("monthly", class.monthly.as_ref()),
    ] {
        let Some(spend) = spend else { continue };
        println!(
            "  {window:<8} spent ${:.4}, {} tokens",
            spend.usd, spend.tokens
        );
    }
    if let Some(caps) = &class.caps {
        print_caps(caps);
    }
}

/// A dollar cap, or `-` when that dimension is uncapped. Uncapped is not
/// zero: printing `$0.00` for an absent cap would read as "spend nothing".
fn opt_usd(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:.4}"))
}

fn opt_count(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

/// A formatted, human-readable rendering of a `Summary` — shared by
/// `mail ai summary` and the terminal frame of `mail ai process`'s stream.
fn print_summary(summary: &Summary) {
    println!("message_id: {}", summary.message_id);
    println!("status:     {}", status_name(summary.status()));
    if let Some(tl_dr) = &summary.tl_dr {
        println!("tl;dr:      {tl_dr}");
    }
    if let Some(category) = &summary.category {
        println!("category:   {category}");
    }
    if let Some(priority) = &summary.priority {
        println!("priority:   {priority}");
    }
    if let Some(sentiment) = &summary.sentiment {
        println!("sentiment:  {sentiment}");
    }
    if let Some(needs_reply) = summary.needs_reply {
        println!("needs_reply: {needs_reply}");
    }
    if !summary.suggested_tags.is_empty() {
        println!("tags:       {}", summary.suggested_tags.join(", "));
    }
    if let Some(text) = &summary.summary {
        println!("\nsummary:\n{text}");
    }
    if !summary.key_points.is_empty() {
        println!("\nkey points:");
        for point in &summary.key_points {
            println!("  - {point}");
        }
    }
    if !summary.todos.is_empty() {
        println!("\ntodos:");
        for todo in &summary.todos {
            let due = todo.due.as_deref().unwrap_or("no due date");
            let owner = todo.owner.as_deref().unwrap_or("unassigned");
            println!("  - {} (due: {due}, owner: {owner})", todo.text);
        }
    }
    if let Some(reply) = &summary.suggested_reply {
        println!("\nsuggested reply:\n{reply}");
    }
}

fn status_name(status: rmail_proto::v1::SummaryStatus) -> &'static str {
    status.as_str_name().trim_start_matches("SUMMARY_STATUS_")
}

/// `Summary` as `serde_json::Value` — the generated proto type does not
/// derive `Serialize` (`build.rs` does not enable prost-build's serde
/// support, and `build.rs` is off limits — see that file's own header), so
/// `mail ai summary --json` builds one by hand rather than leaving `--json`
/// unimplemented.
fn summary_to_json(summary: &Summary) -> serde_json::Value {
    serde_json::json!({
        "message_id": summary.message_id,
        "thread_id": summary.thread_id,
        "status": status_name(summary.status()),
        "triage_model": summary.triage_model,
        "tl_dr": summary.tl_dr,
        "sentiment": summary.sentiment,
        "category": summary.category,
        "priority": summary.priority,
        "needs_reply": summary.needs_reply,
        "suggested_tags": summary.suggested_tags,
        "deep_model": summary.deep_model,
        "summary": summary.summary,
        "thread_summary": summary.thread_summary,
        "key_points": summary.key_points,
        "todos": summary.todos.iter().map(|t| serde_json::json!({
            "text": t.text,
            "due": t.due,
            "owner": t.owner,
        })).collect::<Vec<_>>(),
        "entities": summary.entities.iter().map(|e| serde_json::json!({
            "kind": e.kind,
            "value": e.value,
            "iso": e.iso,
            "amount": e.amount,
            "currency": e.currency,
        })).collect::<Vec<_>>(),
        "suggested_reply": summary.suggested_reply,
    })
}

#[cfg(test)]
mod ask_render_tests {
    use super::terminal_safe;

    /// A model steered by a hostile message, or a subject line written by one,
    /// must not be able to drive the terminal it is printed to.
    #[test]
    fn an_ansi_escape_in_model_or_mail_text_never_reaches_the_terminal() {
        // A cursor-up-and-overwrite run: the classic way to make output claim
        // something other than what was actually said.
        let hostile = "Refund confirmed\u{1b}[1A\u{1b}[2K totally legitimate";
        let safe = terminal_safe(hostile);
        assert!(
            !safe.contains('\u{1b}'),
            "an ESC survived into terminal output: {safe:?}"
        );
        assert!(safe.contains("Refund confirmed"), "the text itself is kept");
    }

    /// Bidi overrides reorder what a reader sees without corrupting anything,
    /// which is exactly why a control-character filter alone misses them.
    #[test]
    fn a_bidi_override_cannot_reorder_what_the_user_reads() {
        let spoofed = "paid \u{202e}drac tiderc\u{202c} today";
        let safe = terminal_safe(spoofed);
        assert!(
            !safe.contains('\u{202e}') && !safe.contains('\u{202c}'),
            "a bidi control survived: {safe:?}"
        );
    }

    /// Prose, not a one-line hit: newlines are paragraph breaks and must
    /// survive, which is why this cannot just reuse `search_cli`'s renderer.
    #[test]
    fn newlines_survive_but_tabs_do_not() {
        assert_eq!(terminal_safe("one\ntwo"), "one\ntwo");
        assert_eq!(terminal_safe("a\tb"), "a b");
    }
}
