//! `mail tag`/`mail untag`/`mail tags` — thin gRPC-client verbs over
//! `TagService` (task 55).
//!
//! # Target syntax
//!
//! `mail tag`/`mail untag`'s first positional argument names what the tags
//! apply to, per prd.md's CLI grammar:
//!
//! - a bare integer — a message id (`Target::message_id`).
//! - `thread:<id>` — a whole thread (`Target::thread_id`); every current
//!   *and future* member.
//! - `search:<query>` — recognized as a bulk selection
//!   (`TagService.BulkTag`'s `query` selector), but rejected with a pointer
//!   to `mail tag-bulk` rather than acted on directly here: `BulkTag` needs
//!   an `account_id` this positional form has no field for, so `mail tag
//!   search:"…" <tag>` fails fast with the exact command to use instead of
//!   silently defaulting an account. `mail tag-bulk --query <query>
//!   --account <id> <tag>...` is the working form. `mail untag` recognizes
//!   `search:` only to give the same fail-fast message — `RemoveTag` has no
//!   bulk form on the wire at all (`RemoveTag` takes a single [`Target`],
//!   not a selector), so there is no subcommand to point to there.
//!
//! prd.md also shows `mail tag <id> --thread work` (a boolean `--thread`
//! flag that resolves "the thread this message belongs to"), which this
//! module does not implement: that form needs an extra message → thread
//! lookup this CLI has no RPC for without also depending on `MailService`
//! just for one field already available directly as `thread:<id>` when the
//! caller knows it. `thread:<id>` covers the same target unambiguously; the
//! convenience form is a small, self-contained follow-up if the id is ever
//! genuinely inconvenient to look up first.

use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use rmail_proto::v1::tag_service_client::TagServiceClient;
use rmail_proto::v1::{
    bulk_tag_request, target, AddTagRequest, BulkTagRequest, CreateTagRequest, ListTagsRequest,
    RemoveTagRequest, Tag, TagSyncMode as ProtoTagSyncMode, TagWithCount, Target,
};
use tokio_stream::StreamExt;

/// `mail tag <target> <tag>...` flags.
#[derive(Debug, Args)]
pub struct TagArgs {
    /// Target: a message id, `thread:<id>`, or `search:<query>`.
    target: String,
    /// Tag name(s) to apply; created on demand.
    #[arg(required = true)]
    tags: Vec<String>,
}

/// `mail untag <target> <tag>...` flags.
#[derive(Debug, Args)]
pub struct UntagArgs {
    /// Target: a message id or `thread:<id>` (no bulk form — see the module
    /// docs).
    target: String,
    #[arg(required = true)]
    tags: Vec<String>,
}

/// `mail tags [--account <id>]` — list, or `mail tags create ...`.
#[derive(Debug, Args)]
pub struct TagsArgs {
    #[command(subcommand)]
    action: Option<TagsAction>,
    /// Account whose tags to list. Required when no subcommand is given.
    #[arg(long)]
    account: Option<i64>,
}

#[derive(Debug, Subcommand)]
pub enum TagsAction {
    /// Create a tag, or update an existing one of the same name
    /// (`TagService.CreateTag`).
    Create {
        /// Full hierarchical name (`project/alpha`); ancestor tags are
        /// auto-created.
        name: String,
        #[arg(long)]
        account: i64,
        #[arg(long)]
        color: Option<String>,
        #[arg(long, value_enum)]
        sync: Option<SyncModeArg>,
        /// Explicit parent tag id, independent of the `/`-name convention.
        #[arg(long)]
        parent: Option<i64>,
    },
}

/// `--sync`'s value, spelled the way a terminal user types it.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SyncModeArg {
    Local,
    Imap,
    Auto,
}

async fn client(socket: &Path) -> Result<TagServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(TagServiceClient::new(channel))
}

/// Parse a `mail tag`/`mail untag` target. `allow_bulk = false` rejects a
/// `search:` prefix (`mail untag` has no bulk form — see the module docs).
#[derive(Debug)]
enum ParsedTarget {
    Direct(Target),
    Bulk(String),
}

fn parse_target(raw: &str, allow_bulk: bool) -> Result<ParsedTarget> {
    if let Some(thread_id) = raw.strip_prefix("thread:") {
        let id: i64 = thread_id
            .parse()
            .with_context(|| format!("invalid thread id in {raw:?}"))?;
        return Ok(ParsedTarget::Direct(Target {
            of: Some(target::Of::ThreadId(id)),
        }));
    }
    if let Some(query) = raw.strip_prefix("search:") {
        if !allow_bulk {
            bail!("`untag` has no bulk form; give a message id or thread:<id>, not {raw:?}");
        }
        return Ok(ParsedTarget::Bulk(query.to_owned()));
    }
    let id: i64 = raw
        .parse()
        .with_context(|| format!("{raw:?} is not a message id, thread:<id>, or search:<query>"))?;
    Ok(ParsedTarget::Direct(Target {
        of: Some(target::Of::MessageId(id)),
    }))
}

/// `mail tag <target> <tag>...`.
pub async fn tag(socket: &Path, args: TagArgs) -> Result<()> {
    match parse_target(&args.target, true)? {
        ParsedTarget::Direct(target) => {
            let response = client(socket)
                .await?
                .add_tag(AddTagRequest {
                    target: Some(target),
                    names: args.tags,
                })
                .await
                .context("AddTag RPC failed")?
                .into_inner();
            println!("applied {} tag(s)", response.applications.len());
            for application in &response.applications {
                if let Some(tag) = &application.tag {
                    println!("  {}", tag.name);
                }
            }
        }
        ParsedTarget::Bulk(query) => {
            bail!(
                "`mail tag search:\"{query}\" <tag>` needs an account — use \
                 `mail tag-bulk --query \"{query}\" --account <id> <tag>...` instead"
            );
        }
    }
    Ok(())
}

/// `mail tag-bulk --query <query> --account <id> <tag>...` — the
/// account-scoped bulk form. A separate subcommand (rather than folding
/// `--account` into [`TagArgs`] for every call) because a direct
/// message/thread target resolves its account server-side and should never
/// need the flag.
pub async fn bulk_tag(socket: &Path, account: i64, query: String, tags: Vec<String>) -> Result<()> {
    let response = client(socket)
        .await?
        .bulk_tag(BulkTagRequest {
            account_id: account,
            names: tags,
            selector: Some(bulk_tag_request::Selector::Query(query)),
        })
        .await
        .context("BulkTag RPC failed")?
        .into_inner();
    println!(
        "tagged {} of {} matching message(s)",
        response.applied, response.message_count
    );
    Ok(())
}

/// `mail untag <target> <tag>...`.
pub async fn untag(socket: &Path, args: UntagArgs) -> Result<()> {
    // `parse_target(_, allow_bulk: false)` only ever produces `Direct` or an
    // `Err` (a `search:` target is rejected inside `parse_target` itself) --
    // the `Bulk` arm below is therefore dead, but handled as an ordinary
    // `bail!` rather than `unreachable!()` so a future change to
    // `parse_target` that broke that invariant would fail loudly as a CLI
    // error, not a panic.
    let target = match parse_target(&args.target, false)? {
        ParsedTarget::Direct(target) => target,
        ParsedTarget::Bulk(query) => {
            bail!("internal error: unexpected bulk target {query:?} in `mail untag`")
        }
    };
    client(socket)
        .await?
        .remove_tag(RemoveTagRequest {
            target: Some(target),
            names: args.tags,
        })
        .await
        .context("RemoveTag RPC failed")?;
    println!("removed");
    Ok(())
}

/// `mail tags` / `mail tags create ...`.
pub async fn tags(socket: &Path, args: TagsArgs) -> Result<()> {
    match args.action {
        None => {
            let account = args.account.context("`mail tags` needs --account <id>")?;
            let response = client(socket)
                .await?
                .list_tags(ListTagsRequest {
                    account_id: account,
                })
                .await
                .context("ListTags RPC failed")?
                .into_inner();
            if response.tags.is_empty() {
                println!("no tags");
                return Ok(());
            }
            for with_count in &response.tags {
                print_tag_with_count(with_count);
            }
        }
        Some(TagsAction::Create {
            name,
            account,
            color,
            sync,
            parent,
        }) => {
            let tag = client(socket)
                .await?
                .create_tag(CreateTagRequest {
                    account_id: account,
                    name,
                    color,
                    sync_mode: sync.map(|s| sync_mode_to_proto(s) as i32),
                    parent_id: parent,
                })
                .await
                .context("CreateTag RPC failed")?
                .into_inner();
            print_tag(&tag);
        }
    }
    Ok(())
}

fn sync_mode_to_proto(mode: SyncModeArg) -> ProtoTagSyncMode {
    match mode {
        SyncModeArg::Local => ProtoTagSyncMode::Local,
        SyncModeArg::Imap => ProtoTagSyncMode::Imap,
        SyncModeArg::Auto => ProtoTagSyncMode::Auto,
    }
}

fn sync_mode_name(mode: i32) -> &'static str {
    ProtoTagSyncMode::try_from(mode)
        .unwrap_or(ProtoTagSyncMode::Unspecified)
        .as_str_name()
        .trim_start_matches("TAG_SYNC_MODE_")
}

fn print_tag(tag: &Tag) {
    println!(
        "{:<6} {:<28} {:<8} {}",
        tag.id,
        tag.name,
        sync_mode_name(tag.sync_mode),
        tag.color.as_deref().unwrap_or("-"),
    );
}

fn print_tag_with_count(with_count: &TagWithCount) {
    let Some(tag) = &with_count.tag else {
        return;
    };
    println!(
        "{:<6} {:<28} {:<8} {:<10} {}",
        tag.id,
        tag.name,
        sync_mode_name(tag.sync_mode),
        with_count.message_count,
        tag.color.as_deref().unwrap_or("-"),
    );
}

/// Drain a `SuggestTags` stream and print each pending suggestion —
/// `mail suggest-tags <id>` (task 57 owns generating them; this only
/// displays what is already pending, per `TagService.SuggestTags`'s own
/// contract).
pub async fn suggest_tags(socket: &Path, message_id: i64) -> Result<()> {
    let mut stream = client(socket)
        .await?
        .suggest_tags(rmail_proto::v1::SuggestTagsRequest { message_id })
        .await
        .context("SuggestTags RPC failed")?
        .into_inner();
    let mut any = false;
    while let Some(item) = stream.next().await {
        let suggestion = item.context("suggestion stream ended with an error")?;
        any = true;
        let name = suggestion
            .tag
            .as_ref()
            .map(|t| t.name.as_str())
            .unwrap_or("?");
        println!(
            "{:<6} {:<28} {:.2}  {}",
            suggestion.message_tag_id, name, suggestion.confidence, suggestion.rationale
        );
    }
    if !any {
        println!("no pending suggestions");
    }
    Ok(())
}

/// `mail accept-tags`/`mail reject-tags <message_tag_id>...`.
pub async fn resolve_suggestions(socket: &Path, ids: Vec<i64>, accept: bool) -> Result<()> {
    let mut c = client(socket).await?;
    for message_tag_id in ids {
        c.resolve_suggestion(rmail_proto::v1::ResolveSuggestionRequest {
            message_tag_id,
            accept,
        })
        .await
        .with_context(|| format!("ResolveSuggestion RPC failed for {message_tag_id}"))?;
    }
    println!("{}", if accept { "accepted" } else { "rejected" });
    Ok(())
}

#[cfg(test)]
mod tests;
