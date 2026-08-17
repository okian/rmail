//! `mail index` and `mail entities` — thin gRPC-client verbs over
//! `IndexService` (task 24).
//!
//! # Which verb does what, because two of them destroy data
//!
//! The CLI is where the difference between `reindex` and `rebuild` has to be
//! obvious, since it is the surface a tired operator reaches for at the wrong
//! moment. `reindex` re-enqueues what is stale and is free over a current
//! index. `rebuild` deletes the derived data for the stages it is given, and
//! search over those stages returns nothing until the drain finishes — so it
//! requires `--all` or an explicit `--kind`, plus `--yes` for an unattended
//! run, and otherwise asks on the terminal.
//!
//! `verify` reports and repairs nothing; `gc` deletes only rows whose parent is
//! already gone. `start`/`stop` control the background worker and nothing else:
//! the queue is durable, so a stopped daemon accumulates work rather than
//! losing it, and `run` still drains on demand while it is stopped.
//!
//! # Progress
//!
//! `run`/`reindex`/`rebuild`/`embed` stream progress frames. They are printed
//! as a single rewritten line on a terminal and as plain lines when redirected,
//! because a progress bar written into a log file is unreadable and a log file
//! is where CI puts this.

use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use rmail_proto::v1::index_service_client::IndexServiceClient;
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::{
    IndexGcRequest, IndexKind as ProtoKind, IndexKindStatus, IndexProgress, IndexStatusRequest,
    ListEntitiesRequest, RebuildRequest, ReindexMode, ReindexRequest, SearchEntitiesRequest,
    SetIndexPausedRequest, VerifyIndexRequest,
};
use tokio_stream::StreamExt;

/// `mail index <verb>`.
#[derive(Debug, Subcommand)]
pub enum IndexAction {
    /// Per-stage coverage, queue depth, model and lag
    /// (`IndexService.Status`).
    Status,
    /// Drain the queue once, streaming progress (`IndexService.Reindex` in
    /// drain mode). Runs even while the background worker is stopped.
    Run {
        /// Stop after this many jobs. 0 drains until the queue is empty.
        #[arg(long, default_value_t = 0)]
        max_jobs: i64,
    },
    /// Start the background indexing worker (`IndexService.SetPaused`).
    Start,
    /// Stop the background indexing worker. Queued work is durable and is
    /// picked up when it starts again.
    Stop,
    /// Re-enqueue whatever is stale and drain it. Free over a current index —
    /// content already indexed against the same hash and model dedups away.
    Reindex {
        /// Restrict to one stage; repeatable. Default: every stage.
        #[arg(long = "kind", value_enum)]
        kinds: Vec<KindArg>,
        /// Only mail that arrived at or after this unix timestamp.
        #[arg(long)]
        since: Option<i64>,
        /// Only this mailbox.
        #[arg(long)]
        mailbox: Option<i64>,
        /// Only this message.
        #[arg(long)]
        message: Option<i64>,
        /// Only this account.
        #[arg(long)]
        account: Option<i64>,
        #[arg(long, default_value_t = 0)]
        max_jobs: i64,
    },
    /// DELETE the index for the named stages and recompute it from scratch.
    ///
    /// Not a stronger `reindex`: search over the rebuilt stages returns
    /// nothing until the drain catches up, which for a large mailbox with
    /// embeddings on is hours. Use it when the extractor or the model changed
    /// underneath everything already recorded.
    Rebuild {
        /// Rebuild every stage. Required unless `--kind` is given.
        #[arg(long)]
        all: bool,
        /// Rebuild only this stage; repeatable.
        #[arg(long = "kind", value_enum)]
        kinds: Vec<KindArg>,
        /// Skip the confirmation prompt — for scripts, which have no terminal
        /// to answer on.
        #[arg(long, short = 'y')]
        yes: bool,
        #[arg(long, default_value_t = 0)]
        max_jobs: i64,
    },
    /// Report drift between what the index records and what it holds. Changes
    /// nothing (`IndexService.Verify`).
    Verify,
    /// Delete index rows whose parent is gone (`IndexService.Gc`).
    ///
    /// Always sweeps the search caches back to their configured bounds too;
    /// every row that removes is expired or unreachable.
    Gc {
        /// Additionally drop *every* cached row, including compiled
        /// natural-language query plans.
        ///
        /// Not needed in normal operation — the search caches invalidate
        /// structurally, so new mail or a retuned ranker already moves the key
        /// rather than leaving a stale answer readable. Use it when you want a
        /// clean slate: a restored backup, or ruling a cache out of a bug.
        /// Each discarded query plan is a paid Claude call that will be paid
        /// again.
        #[arg(long)]
        purge_caches: bool,
    },
    /// Embedding maintenance (`IndexService.Reindex` in backfill mode).
    Embed {
        /// Re-embed every already-chunked message whose vectors are missing,
        /// stale, or from another model. Currently the only mode, and required
        /// so the bare verb never silently does something else later. A mailbox
        /// where `[index.semantic]` has only just been switched on has no
        /// chunks yet — that one is `mail index reindex --kind semantic`.
        #[arg(long)]
        backfill: bool,
        #[arg(long, default_value_t = 0)]
        max_jobs: i64,
    },
}

/// `mail entities <kind>`.
#[derive(Debug, Args)]
pub struct EntitiesArgs {
    /// Entity kind: email, phone, url, amount, date, tracking_no, order_id,
    /// invoice_id, iban. Required for a listing; with `--search` it narrows
    /// the search to that kind instead.
    kind: Option<String>,
    /// Only entities whose canonical form contains this, case-insensitively
    /// (listing only).
    #[arg(long)]
    value: Option<String>,
    /// Search across kinds instead of listing one
    /// (`SearchService.SearchEntities`), printing the mail behind each hit.
    ///
    /// The listing is index maintenance — "what did the extractor produce for
    /// this kind". This is the question a person actually has: "where does
    /// this number turn up".
    #[arg(long)]
    search: Option<String>,
    /// Restrict a search to one account.
    #[arg(long)]
    account: Option<i64>,
    /// Page size.
    #[arg(long, default_value_t = 50)]
    limit: i64,
}

/// A stage, spelled the way a terminal user types it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Extract,
    Lexical,
    Entities,
    Semantic,
}

impl KindArg {
    fn to_proto(self) -> ProtoKind {
        match self {
            Self::Extract => ProtoKind::Extract,
            Self::Lexical => ProtoKind::Lexical,
            Self::Entities => ProtoKind::Entities,
            Self::Semantic => ProtoKind::Semantic,
        }
    }
}

fn kinds(values: &[KindArg]) -> Vec<i32> {
    values.iter().map(|k| k.to_proto() as i32).collect()
}

async fn client(socket: &Path) -> Result<IndexServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(IndexServiceClient::new(channel))
}

/// Run one `mail index` verb.
///
/// # Errors
///
/// A connection or RPC failure, or a refused destructive verb.
pub async fn run(socket: &Path, action: IndexAction) -> Result<()> {
    match action {
        IndexAction::Status => status(socket).await,
        IndexAction::Run { max_jobs } => {
            reindex(
                socket,
                ReindexRequest {
                    mode: ReindexMode::Drain as i32,
                    max_jobs,
                    ..ReindexRequest::default()
                },
            )
            .await
        }
        IndexAction::Start => set_paused(socket, false).await,
        IndexAction::Stop => set_paused(socket, true).await,
        IndexAction::Reindex {
            kinds: wanted,
            since,
            mailbox,
            message,
            account,
            max_jobs,
        } => {
            reindex(
                socket,
                ReindexRequest {
                    mode: ReindexMode::Selection as i32,
                    kinds: kinds(&wanted),
                    account_id: account,
                    mailbox_id: mailbox,
                    message_id: message,
                    since,
                    max_jobs,
                },
            )
            .await
        }
        IndexAction::Rebuild {
            all,
            kinds: wanted,
            yes,
            max_jobs,
        } => rebuild(socket, all, &wanted, yes, max_jobs).await,
        IndexAction::Verify => verify(socket).await,
        IndexAction::Gc { purge_caches } => gc(socket, purge_caches).await,
        IndexAction::Embed { backfill, max_jobs } => {
            if !backfill {
                bail!("`mail index embed` needs --backfill; it has no other mode yet");
            }
            reindex(
                socket,
                ReindexRequest {
                    mode: ReindexMode::EmbedBackfill as i32,
                    max_jobs,
                    ..ReindexRequest::default()
                },
            )
            .await
        }
    }
}

async fn status(socket: &Path) -> Result<()> {
    let status = client(socket)
        .await?
        .status(IndexStatusRequest {})
        .await
        .context("index status RPC failed")?
        .into_inner();

    println!(
        "Messages      {}   worker {}",
        status.messages,
        if status.paused { "stopped" } else { "running" }
    );
    let coverage: Vec<String> = status.kinds.iter().map(coverage_cell).collect();
    println!("Coverage      {}", coverage.join("   "));
    println!(
        "Queue         {} ready   {} backing off   {} running   {} quarantined",
        status.queue_ready, status.queue_backing_off, status.queue_leased, status.queue_dead
    );
    println!(
        "Model         {} ({}d){}   chunks {}   vectors {}",
        status.model,
        status.dim,
        if status.semantic_enabled {
            ""
        } else {
            " [semantic off]"
        },
        status.chunks,
        status.vectors
    );
    let lag: Vec<String> = status
        .kinds
        .iter()
        .map(|k| match k.lag_seconds {
            Some(seconds) => format!("{} {seconds}s", kind_name(k.kind)),
            None => format!("{} —", kind_name(k.kind)),
        })
        .collect();
    println!("Lag           {}", lag.join("   "));
    if let Some(cache) = status.cache {
        // `corpus_version` first because it is the number that explains the
        // rest: every cached result page is keyed on it, so an operator asking
        // "why is search still returning the old ordering" reads this line and
        // knows whether anything could still be served from before the change.
        println!(
            "Corpus        version {}   changed {}",
            cache.corpus_version,
            ago(cache.corpus_changed_at)
        );
        println!(
            "Caches        {} plans ({} hits)   {} query vectors ({} hits)   {} results ({} \
             hits{})",
            cache.query_plans,
            cache.query_plan_uses,
            cache.embeddings,
            cache.embedding_uses,
            cache.results,
            cache.result_uses,
            if cache.stale_results > 0 {
                format!(", {} stale", cache.stale_results)
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

/// A unix timestamp as "12s ago", for a status line a human reads.
fn ago(timestamp: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let seconds = now.saturating_sub(timestamp);
    if seconds < 0 {
        // A clock that moved backwards. Reporting a negative age would read as
        // a bug in this line rather than in the clock.
        return "just now".to_owned();
    }
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// One stage's coverage cell, with the reason it is zero when there is one.
fn coverage_cell(kind: &IndexKindStatus) -> String {
    if kind.enabled {
        format!("{} {:.1}%", kind_name(kind.kind), kind.coverage * 100.0)
    } else {
        format!("{} off", kind_name(kind.kind))
    }
}

fn kind_name(kind: i32) -> String {
    ProtoKind::try_from(kind)
        .map(|k| {
            k.as_str_name()
                .trim_start_matches("INDEX_KIND_")
                .to_lowercase()
        })
        .unwrap_or_else(|_| format!("kind_{kind}"))
}

async fn set_paused(socket: &Path, paused: bool) -> Result<()> {
    let response = client(socket)
        .await?
        .set_paused(SetIndexPausedRequest { paused })
        .await
        .context("index set-paused RPC failed")?
        .into_inner();
    println!(
        "background indexing {}",
        if response.paused {
            "stopped"
        } else {
            "running"
        }
    );
    Ok(())
}

async fn reindex(socket: &Path, request: ReindexRequest) -> Result<()> {
    let stream = client(socket)
        .await?
        .reindex(request)
        .await
        .context("reindex RPC failed")?
        .into_inner();
    follow(stream).await
}

async fn rebuild(
    socket: &Path,
    all: bool,
    wanted: &[KindArg],
    yes: bool,
    max_jobs: i64,
) -> Result<()> {
    // Neither flag means the operator has not said *what* to destroy, and
    // defaulting a wipe to "everything" is exactly the accident this guard
    // exists to prevent.
    if !all && wanted.is_empty() {
        bail!("`mail index rebuild` needs --all or at least one --kind: it deletes the index");
    }
    if all && !wanted.is_empty() {
        bail!("--all and --kind contradict each other; pass one or the other");
    }
    let scope = if all {
        "every stage".to_owned()
    } else {
        wanted
            .iter()
            .map(|k| format!("{k:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !yes {
        confirm(&scope)?;
    }

    let stream = client(socket)
        .await?
        .rebuild(RebuildRequest {
            kinds: kinds(wanted),
            confirm: true,
            max_jobs,
        })
        .await
        .context("rebuild RPC failed")?
        .into_inner();
    follow(stream).await
}

/// Ask before wiping. A non-terminal stdin means a script that never passed
/// `--yes`, and guessing "yes" for it would make the flag decorative.
fn confirm(scope: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("rebuild deletes the index for {scope}; pass --yes to confirm non-interactively");
    }
    print!("Delete and recompute the index for {scope}? [y/N] ");
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("reading confirmation")?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        bail!("cancelled; nothing was deleted");
    }
    Ok(())
}

/// Print a progress stream until it ends.
async fn follow(mut stream: tonic::Streaming<IndexProgress>) -> Result<()> {
    let interactive = std::io::stdout().is_terminal();
    let mut last: Option<IndexProgress> = None;
    while let Some(frame) = stream.next().await {
        let frame = frame.context("index progress stream failed")?;
        if frame.dropped > 0 && last.is_none() {
            println!("dropped {} index rows", frame.dropped);
        }
        let line = format!(
            "{} done   {} failed   {} queued",
            frame.completed, frame.failed, frame.remaining
        );
        if frame.done {
            if interactive {
                println!("\r{line}          ");
            } else {
                println!("{line}");
            }
        } else if interactive {
            print!("\r{line}          ");
            std::io::stdout().flush().ok();
        } else {
            println!("{line}");
        }
        let done = frame.done;
        last = Some(frame);
        if done {
            return Ok(());
        }
    }
    // No `done` frame: the daemon stopped, or the deadline passed. Saying so
    // matters — the counts printed above are a prefix of the work, not a total.
    match last {
        Some(_) => bail!("the index stream ended before the pass finished"),
        None => bail!("the index stream ended without reporting anything"),
    }
}

async fn verify(socket: &Path) -> Result<()> {
    let drift = client(socket)
        .await?
        .verify(VerifyIndexRequest {})
        .await
        .context("index verify RPC failed")?
        .into_inner();

    if drift.clean {
        println!("index clean");
        return Ok(());
    }
    for (label, count) in [
        ("state/content-hash drift", drift.content_hash_drift),
        ("extracted text with no state row", drift.extract_missing),
        (
            "messages missing from the lexical index",
            drift.lexical_missing,
        ),
        ("lexical rows for deleted mail", drift.lexical_orphaned),
        ("entities with no mention left", drift.entity_orphaned),
        ("chunks never embedded", drift.chunks_unembedded),
        ("chunks with no vector", drift.chunks_unvectored),
        ("chunks from another model", drift.chunks_wrong_model),
        ("chunks whose text moved", drift.chunks_stale),
        ("vectors with no chunk", drift.vectors_orphaned),
        ("stale message centroids", drift.message_vectors_stale),
        ("quarantined jobs", drift.quarantined),
    ] {
        if count > 0 {
            println!("{count:>8}  {label}");
        }
    }
    println!("\nrun `mail index reindex` to repair drift, `mail index gc` to remove orphans");
    Ok(())
}

async fn gc(socket: &Path, purge_caches: bool) -> Result<()> {
    let report = client(socket)
        .await?
        .gc(IndexGcRequest {
            purge_search_caches: purge_caches,
        })
        .await
        .context("index gc RPC failed")?
        .into_inner();
    let index_rows = report.entities + report.vectors + report.lexical_rows + report.content_rows;
    let cache_rows = report.cache_results + report.cache_embeddings + report.cache_query_plans;
    if index_rows == 0 && cache_rows == 0 {
        println!("nothing to collect");
        return Ok(());
    }
    if index_rows > 0 {
        println!(
            "removed {} orphaned entities, {} vectors, {} lexical rows, {} content rows",
            report.entities, report.vectors, report.lexical_rows, report.content_rows
        );
    }
    if cache_rows > 0 {
        println!(
            "search caches: {} result pages, {} query vectors, {} query plans",
            report.cache_results, report.cache_embeddings, report.cache_query_plans
        );
    }
    Ok(())
}

/// `mail entities <kind>`, or `mail entities --search <text>`.
///
/// # Errors
///
/// A connection or RPC failure; a missing kind on a listing; an unknown kind
/// is rejected by the daemon.
pub async fn entities(socket: &Path, args: EntitiesArgs) -> Result<()> {
    if let Some(query) = args.search.clone() {
        return search_entities(socket, &query, &args).await;
    }
    let Some(kind) = args.kind.clone() else {
        anyhow::bail!("name a kind to list, or pass --search <text> to search across kinds");
    };
    let response = client(socket)
        .await?
        .list_entities(ListEntitiesRequest {
            kind,
            value: args.value,
            limit: args.limit,
        })
        .await
        .context("list entities RPC failed")?
        .into_inner();

    if response.entities.is_empty() {
        println!("no entities of that kind");
        return Ok(());
    }
    for entity in response.entities {
        println!(
            "{:<8} {:<40} {:>5} mentions in {:>5} messages",
            entity.entity_id, entity.norm, entity.mentions, entity.messages
        );
    }
    Ok(())
}

/// The `--search` half of `mail entities`.
async fn search_entities(socket: &Path, query: &str, args: &EntitiesArgs) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let response = SearchServiceClient::new(channel)
        .search_entities(SearchEntitiesRequest {
            query: query.to_owned(),
            kinds: args.kind.clone().into_iter().collect(),
            account_id: args.account.unwrap_or_default(),
            since: 0,
            limit: args.limit,
        })
        .await
        .context("SearchEntities RPC failed")?
        .into_inner();

    if response.hits.is_empty() {
        println!("no entities matched");
        return Ok(());
    }
    for hit in response.hits {
        println!(
            "{:<8} {:<12} {:<40} {:>4} messages",
            hit.entity_id, hit.kind, hit.norm, hit.messages
        );
        // The mail behind the hit is the whole reason this is a search and not
        // a listing, so it is printed rather than hidden behind a flag.
        for example in hit.examples {
            println!(
                "         {:<10} {}",
                example.message_id,
                if example.subject.is_empty() {
                    "(no subject)"
                } else {
                    &example.subject
                }
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
