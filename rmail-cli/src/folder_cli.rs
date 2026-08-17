//! `mail folder` — smart folders: virtual mailboxes whose membership is a
//! predicate, re-derived on every read (`SavedSearchService`; tasks 35, 58).
//!
//! # One verb, two ways to say what a folder holds
//!
//! `mail folder new "anything from the landlord about the lease"` is prd.md's
//! headline form: plain English, compiled once by Claude into a hybrid plan
//! (`CompileSmartFolder`). `mail folder new --predicate "from:stripe
//! is:unread"` is the deterministic form task 35 shipped
//! (`CreateSmartFolder`), which reaches no model at all.
//!
//! They are two RPCs rather than one because they need different authority —
//! the compiled form spends at the provider and therefore needs `ai.invoke`,
//! which a plain automation token has no reason to hold. That distinction is
//! a scope-table fact, not a CLI one, so this file presents a single verb and
//! picks the call from whether `--predicate` was given.
//!
//! # What is printed, and why the plan goes to stdout here
//!
//! `mail search --nl` prints its compiled plan to *stderr*, so `--json`
//! remains a clean stream of hits. There is no such stream here: defining a
//! folder prints one report, and the compiled plan is the most important part
//! of it — the user is about to leave a standing query running against their
//! mailbox, and the one moment to notice that "the landlord" compiled to
//! `from:landlord` is now.
//!
//! # Membership is never stored, so `members` is always live
//!
//! `mail folder members` streams what the predicate matches *right now* — see
//! `rmail_core::smart_folder`'s module docs. It evaluates nothing and fires no
//! action; `mail folder eval` is the explicit "re-evaluate and fire what is
//! genuinely new" call, and the daemon's background evaluator normally makes
//! even that unnecessary.

use std::path::Path;

use anyhow::{Context, Result};
use rmail_proto::v1::saved_search_service_client::SavedSearchServiceClient;
use rmail_proto::v1::{
    CompileSmartFolderRequest, CompiledSmartFolder, CreateSmartFolderRequest,
    DeleteSmartFolderRequest, EvaluateSmartFolderRequest, ListSmartFolderMembersRequest,
    ListSmartFoldersRequest, SmartFolder,
};
use tokio_stream::StreamExt;

/// `mail folder <action>`.
#[derive(Debug, clap::Subcommand)]
pub enum FolderAction {
    /// Define a smart folder from plain English, or from an operator
    /// predicate with `--predicate`.
    New(NewArgs),
    /// List an account's smart folders.
    List {
        /// Account id.
        #[arg(long)]
        account: i64,
    },
    /// Stream the messages a smart folder currently matches. Runs the
    /// predicate; stores nothing, fires nothing.
    Members {
        /// Folder name.
        name: String,
        /// Account id.
        #[arg(long)]
        account: i64,
        /// Members to stream; 0 streams all of them.
        #[arg(long, default_value_t = 0)]
        limit: u32,
    },
    /// Re-evaluate now instead of waiting for the background pass, firing
    /// auto-tag/notify for members that are genuinely new.
    Eval {
        /// Folder name.
        name: String,
        /// Account id.
        #[arg(long)]
        account: i64,
    },
    /// Delete a folder's definition. No message is touched, because none was
    /// ever moved.
    Rm {
        /// Folder name.
        name: String,
        /// Account id.
        #[arg(long)]
        account: i64,
    },
}

/// `mail folder new` flags.
#[derive(Debug, clap::Args)]
pub struct NewArgs {
    /// What the folder holds, in plain English ("anything from the landlord
    /// about the lease"). Claude compiles it once into a stored hybrid plan.
    ///
    /// Ignored when `--predicate` is given, which is the deterministic form.
    ///
    /// `allow_hyphen_values`, like `mail search`'s own query: a predicate may
    /// legitimately start with the grammar's `-` negation.
    #[arg(allow_hyphen_values = true)]
    description: String,
    /// Folder name. Defaults to a slug of the description.
    #[arg(long)]
    name: Option<String>,
    /// Account id.
    #[arg(long)]
    account: i64,
    /// Define the folder from this operator-DSL predicate instead of
    /// compiling the description. Reaches no model.
    #[arg(long, allow_hyphen_values = true)]
    predicate: Option<String>,
    /// Apply this tag to genuinely new members.
    #[arg(long = "auto-tag")]
    auto_tag: Option<String>,
    /// Publish an event for genuinely new members.
    #[arg(long)]
    notify: bool,
    /// Recompile instead of serving the cached plan for this sentence. Only
    /// meaningful without `--predicate`.
    #[arg(long)]
    refresh: bool,
}

/// Route one `mail folder` invocation.
///
/// # Errors
/// Whatever the RPC returns, plus a connection failure.
pub async fn dispatch(socket: &Path, action: FolderAction) -> Result<()> {
    let channel = crate::client::connect(socket).await?;
    let mut client = SavedSearchServiceClient::new(channel);

    match action {
        FolderAction::New(args) => new(&mut client, args).await,
        FolderAction::List { account } => {
            let response = client
                .list_smart_folders(ListSmartFoldersRequest {
                    account_id: account,
                })
                .await
                .context("ListSmartFolders RPC failed")?
                .into_inner();
            if response.folders.is_empty() {
                println!("no smart folders");
                return Ok(());
            }
            for folder in &response.folders {
                print_folder(folder);
            }
            Ok(())
        }
        FolderAction::Members {
            name,
            account,
            limit,
        } => {
            let mut stream = client
                .list_smart_folder_members(ListSmartFolderMembersRequest {
                    account_id: account,
                    name,
                    limit,
                })
                .await
                .context("ListSmartFolderMembers RPC failed")?
                .into_inner();
            let mut shown = 0usize;
            while let Some(item) = stream.next().await {
                let message = item.context("member stream item failed")?;
                println!(
                    "{:>8}  {:<28}  {}",
                    message.id,
                    truncate(message.from_addr.as_deref().unwrap_or("(no sender)"), 28),
                    truncate(message.subject.as_deref().unwrap_or("(no subject)"), 68)
                );
                shown += 1;
            }
            if shown == 0 {
                println!("no members");
            }
            Ok(())
        }
        FolderAction::Eval { name, account } => {
            let evaluation = client
                .evaluate_smart_folder(EvaluateSmartFolderRequest {
                    account_id: account,
                    name,
                })
                .await
                .context("EvaluateSmartFolder RPC failed")?
                .into_inner();
            println!(
                "members {}  entered {}  departed {}  tagged {}  notified {}",
                evaluation.members,
                evaluation.entered_count,
                evaluation.departed_count,
                evaluation.tagged,
                evaluation.notified
            );
            Ok(())
        }
        FolderAction::Rm { name, account } => {
            client
                .delete_smart_folder(DeleteSmartFolderRequest {
                    account_id: account,
                    name,
                })
                .await
                .context("DeleteSmartFolder RPC failed")?;
            println!("deleted");
            Ok(())
        }
    }
}

/// `mail folder new`, in whichever of its two forms was asked for.
async fn new(
    client: &mut SavedSearchServiceClient<crate::client::Client>,
    args: NewArgs,
) -> Result<()> {
    let name = match args.name {
        Some(name) => name,
        None => slug(&args.description),
    };
    match args.predicate {
        Some(predicate) => {
            let folder = client
                .create_smart_folder(CreateSmartFolderRequest {
                    account_id: args.account,
                    name,
                    predicate,
                    auto_tag: args.auto_tag.unwrap_or_default(),
                    notify: args.notify,
                })
                .await
                .context("CreateSmartFolder RPC failed")?
                .into_inner();
            print_folder(&folder);
        }
        None => {
            let compiled = client
                .compile_smart_folder(CompileSmartFolderRequest {
                    account_id: args.account,
                    name,
                    description: args.description,
                    auto_tag: args.auto_tag.unwrap_or_default(),
                    notify: args.notify,
                    refresh: args.refresh,
                })
                .await
                .context("CompileSmartFolder RPC failed")?
                .into_inner();
            print_compiled(&compiled);
        }
    }
    Ok(())
}

fn print_compiled(compiled: &CompiledSmartFolder) {
    if let Some(plan) = &compiled.plan {
        let source = if plan.cached {
            "cached".to_owned()
        } else {
            plan.model.clone()
        };
        println!("compiled ({source}): {}", plan.compiled);
        if !plan.filters.is_empty() {
            println!("  filters:  {}", plan.filters.join(" "));
        }
        if !plan.semantic_query.is_empty() {
            println!("  ranked:   {}", plan.semantic_query);
        }
        if !plan.notes.is_empty() {
            println!("  reading:  {}", plan.notes);
        }
    }
    if !compiled.semantic_arm {
        // Said plainly rather than buried: the user asked for a folder defined
        // by meaning and got one defined by words. It still works; it recalls
        // less.
        println!(
            "  note:     no embedding was available, so membership uses the filters and \
             full-text match only"
        );
    }
    if let Some(folder) = &compiled.folder {
        print_folder(folder);
    }
}

fn print_folder(folder: &SmartFolder) {
    let mut line = format!("{:<24}  {}", folder.name, folder.predicate);
    if !folder.auto_tag.is_empty() {
        line.push_str(&format!("  [auto-tag {}]", folder.auto_tag));
    }
    if folder.notify {
        line.push_str("  [notify]");
    }
    println!("{line}");
    if !folder.nl_source.is_empty() {
        println!("  from:     {}", folder.nl_source);
    }
}

/// A folder name derived from the description, for the common case where the
/// user did not want to invent one.
///
/// Lowercase, hyphenated, first six words — short enough to type and stable
/// for the same sentence, which matters because the name is the key every
/// other verb here takes.
fn slug(description: &str) -> String {
    let words: Vec<String> = description
        .split_whitespace()
        .take(6)
        .map(|word| {
            word.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect();
    if words.is_empty() {
        // The daemon rejects an empty name with INVALID_ARGUMENT, which is the
        // right error for "a description of nothing but punctuation" — better
        // than this file inventing a name the user never saw.
        return String::new();
    }
    words.join("-")
}

/// Trim `text` to `width` characters, with an ellipsis when it was cut.
fn truncate(text: &str, width: usize) -> String {
    match text.char_indices().nth(width) {
        Some((cut, _)) => format!("{}…", text.get(..cut).unwrap_or_default()),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests;
