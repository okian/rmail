//! `mail find` — a thin gRPC-client verb over `FinderService` (task 59).
//!
//! # Why this one collects the stream instead of printing as it arrives
//!
//! `mail search` prints each hit the moment it comes off the wire, because
//! `SearchService.Search` streams *individual hits* in final rank order and
//! buffering them would throw away the latency the pipeline was built to
//! deliver (see `search_cli`'s own docs).
//!
//! `FinderService.Find` streams something different: each `FindBatch` is a
//! **complete snapshot** of the current top-K, and a later batch can outrank
//! an earlier one — that is the whole reason a bounded top-K heap flushes
//! progressively instead of waiting. Printing every batch would print the
//! same list several times over, in different orders; printing only the first
//! would print a partial ranking. So this module keeps the latest batch and
//! prints once, when the stream ends.
//!
//! The property that made streaming worth having is still there and still
//! belongs to the client that needs it: an interactive picker (task 85's
//! overlay) renders every batch as it lands, replacing its list each time,
//! and gets a first paint long before the scan finishes. A command that is
//! about to be piped into `xargs` wants the finished ranking, not the first
//! frame of it.
//!
//! # `--select --action`
//!
//! prd.md's batch verb: run the query, then apply one action to everything it
//! matched. The action is executed server-side by
//! `FinderService.BatchAction`, which runs it through the same `MailStore`
//! `MailService` does — this module never reaches for `MailService` itself,
//! because "archive these twenty" would then be twenty separate RPCs with
//! twenty separate failure modes and no report of which of them landed.
//!
//! Non-message matches are filtered out here rather than sent and rejected: a
//! `--scope all` query legitimately matches a folder and a tag alongside its
//! messages, and the useful reading of `--action archive` over that is
//! "archive the mail", not "fail because a tag cannot be archived".
//!
//! # The `--json` schema
//!
//! One JSON object per line, hand-written rather than derived from the wire
//! types — `search_cli`'s module docs give the full argument for why (a proto
//! field rename must not silently reshape a documented CLI contract).
//!
//! ```json
//! {
//!   "item_id": 41,
//!   "kind": "message",
//!   "ref_id": 4471,
//!   "score": 138.4,
//!   "text": "Invoice #338 — Acme",
//!   "secondary": "billing@acme.com",
//!   "positions": [0, 8, 9],
//!   "account_id": 1,
//!   "mailbox_id": 2
//! }
//! ```
//!
//! `positions` are **char** offsets into `text`, ascending and deduplicated —
//! never byte offsets. A consumer highlighting them should index
//! `text.chars()`, not `text.as_bytes()`; prd.md's own `id\tkind\ttext +
//! positions` sketch is this, with names.

use std::io::Write as _;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use rmail_proto::v1::finder_service_client::FinderServiceClient;
use rmail_proto::v1::{
    BatchActionRequest, FindBatch, FindRequest, FindResult, FinderScope as ProtoScope,
    FinderStatusRequest, ItemKind as ProtoItemKind,
};
use serde::Serialize;
use tokio_stream::StreamExt;

/// `mail find [flags] [query]`.
#[derive(Debug, Args)]
pub struct FindArgs {
    /// What to look for. A leading `>` searches commands, `#` tags, `@`
    /// contacts, `/` saved searches, `:` folders; the sigil overrides
    /// `--scope`. Omit for the signal-ranked list a picker opens with.
    query: Option<String>,
    /// Which kinds to search when the query carries no sigil.
    #[arg(long, value_enum)]
    scope: Option<ScopeArg>,
    /// Restrict to one account.
    #[arg(long)]
    account: Option<i64>,
    /// Restrict to one folder, by mailbox id (prd.md's `in-folder` scope).
    #[arg(long = "in-folder")]
    in_folder: Option<i64>,
    /// Maximum results. 0 (the default) uses the daemon's
    /// `finder.max_results`.
    #[arg(long, default_value_t = 0)]
    limit: u32,
    /// One JSON object per line instead of the table.
    #[arg(long)]
    json: bool,
    /// Apply `--action` to every message this query matched.
    #[arg(long, requires = "action")]
    select: bool,
    /// The action `--select` applies: archive, delete, read, unread, flag,
    /// unflag.
    #[arg(long, requires = "select")]
    action: Option<String>,
    /// Report how complete and how fresh the index is, and do nothing else.
    #[arg(long, conflicts_with_all = ["select", "json"])]
    status: bool,
    /// Re-derive the index from the source tables before searching.
    #[arg(long)]
    rebuild: bool,
}

/// `--scope`'s vocabulary. Spelled out rather than reusing the proto enum so
/// `--help` prints `messages` rather than `FINDER_SCOPE_MESSAGES`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ScopeArg {
    All,
    Messages,
    Mailboxes,
    Contacts,
    Searches,
    Tags,
    Commands,
}

impl ScopeArg {
    fn into_proto(self) -> ProtoScope {
        match self {
            Self::All => ProtoScope::All,
            Self::Messages => ProtoScope::Messages,
            Self::Mailboxes => ProtoScope::Mailboxes,
            Self::Contacts => ProtoScope::Contacts,
            Self::Searches => ProtoScope::SavedSearches,
            Self::Tags => ProtoScope::Tags,
            Self::Commands => ProtoScope::Commands,
        }
    }
}

/// One result line in `--json` mode. See the module docs for the contract.
#[derive(Debug, Serialize)]
struct JsonItem {
    item_id: i64,
    kind: &'static str,
    ref_id: i64,
    score: f64,
    text: String,
    secondary: String,
    positions: Vec<u32>,
    account_id: i64,
    mailbox_id: i64,
}

/// Run `mail find`.
///
/// # Errors
///
/// Anything that stops the command completing: no daemon, a failed RPC, an
/// unwritable stdout.
pub async fn find(socket: &Path, args: FindArgs) -> Result<()> {
    let channel = crate::client::connect(socket).await?;
    let mut client = FinderServiceClient::new(channel);

    if args.status {
        let status = client
            .index_status(FinderStatusRequest {})
            .await
            .context("IndexStatus RPC failed")?
            .into_inner();
        let mut out = std::io::stdout().lock();
        writeln!(out, "entries     {}", status.entries)?;
        writeln!(out, "memory      {} KiB", status.bytes / 1024)?;
        writeln!(out, "pending     {}", status.pending)?;
        writeln!(out, "rejected    {}", status.rejected)?;
        writeln!(
            out,
            "refreshed   {}",
            if status.refreshed_at == 0 {
                "never".to_owned()
            } else {
                status.refreshed_at.to_string()
            }
        )?;
        return Ok(());
    }

    if args.rebuild {
        let rebuilt = client
            .rebuild_index(rmail_proto::v1::FinderRebuildRequest {})
            .await
            .context("RebuildIndex RPC failed")?
            .into_inner();
        let mut out = std::io::stdout().lock();
        writeln!(out, "rebuilt: {} entries", rebuilt.entries)?;
    }

    let request = FindRequest {
        query: args.query.clone().unwrap_or_default(),
        scope: args
            .scope
            .map_or(ProtoScope::Unspecified, ScopeArg::into_proto) as i32,
        account_id: args.account.unwrap_or(0),
        mailbox_id: args.in_folder.unwrap_or(0),
        limit: args.limit,
        // Highlights are only rendered by the JSON path (the table has nowhere
        // to put them, and `search_cli`'s ANSI-safety argument applies here
        // too), so nothing else pays for the extra traceback per row.
        // `wants_json`, not `args.json`: a `--format json` caller must get the
        // same `positions` a `--json` one does, or the two spellings of the
        // same flag would produce different objects.
        with_positions: crate::format::wants_json(args.json),
    };

    let mut stream = client
        .find(request)
        .await
        .context("Find RPC failed")?
        .into_inner();

    // See the module docs: batches are snapshots, so the last one wins.
    let mut latest: Vec<FindResult> = Vec::new();
    let mut superseded = false;
    while let Some(item) = stream.next().await {
        let batch: FindBatch = item.context("the Find stream failed mid-flight")?;
        superseded = batch.superseded;
        latest = batch.results;
    }
    if superseded {
        // The daemon serves one interactive finder slot, so a concurrent
        // `mail find` (or a TUI picker) can legitimately cut this one short.
        // A short *list* is a note; a short list about to be archived or
        // deleted is a refusal. `--select` means "everything this matched",
        // and a knowingly-incomplete answer cannot honour that — acting on it
        // would quietly do less than the user asked, with no way to tell
        // afterwards which messages were missed.
        if let Some(action) = args.action.as_deref() {
            return Err(anyhow!(
                "refusing to {action} a partial result set: this query was superseded by a \
                 newer one before it finished. Re-run it when no other finder query is in \
                 flight."
            ));
        }
        eprintln!("note: this query was superseded by a newer one; results may be incomplete");
    }

    if let Some(action) = args.action.as_deref() {
        return apply(&mut client, action, &latest).await;
    }

    // `wants_json`, not `args.json`: a `--format json` caller must not be
    // handed the table. The sequence is written through `crate::format` so
    // `--format json` gets one array and `--json`/`--format ndjson` get the
    // line-per-item stream this flag has always emitted — and so every string
    // goes through the escaper rather than raw `serde_json`.
    if crate::format::wants_json(args.json) {
        // An explicit `--format json` wins over the legacy flag; see
        // `search_cli` for the same rule.
        let mut seq = if args.json && !crate::format::current().is_structured() {
            crate::format::JsonSeq::ndjson()
        } else {
            crate::format::JsonSeq::open()
        };
        for item in &latest {
            seq.write(&to_json(item))?;
        }
        return seq.finish();
    }

    let mut out = std::io::stdout().lock();
    {
        for item in &latest {
            writeln!(
                out,
                "{:<12} {:>8}  {}{}",
                kind_name(item.kind),
                item.ref_id,
                sanitize(&item.primary_text),
                if item.secondary.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", sanitize(&item.secondary))
                }
            )?;
        }
        if latest.is_empty() {
            writeln!(out, "no matches")?;
        }
    }
    out.flush()?;
    Ok(())
}

/// Apply `--action` to every message the query matched.
async fn apply(
    client: &mut FinderServiceClient<crate::client::Client>,
    action: &str,
    results: &[FindResult],
) -> Result<()> {
    let ref_ids: Vec<i64> = results
        .iter()
        .filter(|item| item.kind == ProtoItemKind::Message as i32)
        .map(|item| item.ref_id)
        .collect();
    if ref_ids.is_empty() {
        return Err(anyhow!(
            "nothing to act on: this query matched no messages (--action applies to mail only)"
        ));
    }
    let selected = ref_ids.len();
    let response = client
        .batch_action(BatchActionRequest {
            action: action.to_owned(),
            ref_ids,
            // Stated, not implied: the daemon refuses a batch that does not
            // say what its ids are ids of, because `ref_id` spaces overlap
            // across kinds. The filter above is what makes this true.
            kind: ProtoItemKind::Message as i32,
        })
        .await
        .with_context(|| format!("BatchAction RPC failed for {action:?}"))?
        .into_inner();

    let mut out = std::io::stdout().lock();
    writeln!(out, "{}: {} of {selected}", action, response.applied)?;
    if !response.not_found.is_empty() {
        // Reported rather than swallowed: a picker's selection can outlive
        // the mail it names, and the caller should know which ids were
        // already gone.
        writeln!(
            out,
            "not found: {}",
            response
                .not_found
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    out.flush()?;
    Ok(())
}

fn to_json(item: &FindResult) -> JsonItem {
    JsonItem {
        item_id: item.item_id,
        kind: kind_name(item.kind),
        ref_id: item.ref_id,
        score: item.score,
        text: item.primary_text.clone(),
        secondary: item.secondary.clone(),
        positions: item.positions.clone(),
        account_id: item.account_id,
        mailbox_id: item.mailbox_id,
    }
}

/// The stable `--json` spelling of a kind. Written out rather than derived
/// from the generated enum's `as_str_name`, which would print
/// `ITEM_KIND_MESSAGE` and would change shape if the enum were renamed.
fn kind_name(kind: i32) -> &'static str {
    match ProtoItemKind::try_from(kind).unwrap_or(ProtoItemKind::Unspecified) {
        ProtoItemKind::Message => "message",
        ProtoItemKind::Mailbox => "mailbox",
        ProtoItemKind::Contact => "contact",
        ProtoItemKind::SavedSearch => "saved_search",
        ProtoItemKind::Tag => "tag",
        ProtoItemKind::Command => "command",
        ProtoItemKind::Unspecified => "unknown",
    }
}

/// Strip control characters from text that came out of a message.
///
/// The table writes indexed text close to verbatim, and a subject line is
/// attacker-controlled: an `ESC` byte in it would be interpreted by the
/// terminal, not displayed. `search_cli::render_snippet` makes the same
/// argument at length; the rule is identical here, and simpler, because no
/// offsets are being rendered alongside — `--json` (which is where positions
/// go) is escaped by `serde_json` and needs no sanitizing at all.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .filter(|c| !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests;
