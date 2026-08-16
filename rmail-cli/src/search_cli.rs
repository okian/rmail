//! `mail search`/`mail similar` — thin gRPC-client verbs over
//! `SearchService.Search`/`Semantic` (task 33). Every RPC field this module
//! sends comes straight from a CLI flag; every field the daemon sends back
//! is either printed as-is or reshaped into the stable `--json` contract
//! below. No ranking, parsing, or presentation decision is made here that
//! the daemon has already made — this file's whole job is transport plus two
//! output renderers.
//!
//! # Sigils are the server's job
//!
//! `~contract termination clause` (force semantic) and `=Q3 report` (force
//! lexical) are parsed by `rmail-core::query::parse` as [`Mode`](
//! rmail_core::query::parse::Mode) sigils on individual terms/phrases — a
//! per-token decision the operator grammar makes while walking the raw
//! string. This module never inspects `query` for a leading `~`/`=` at all;
//! it passes whatever the user typed straight through as
//! `SearchRequest.query` and lets the daemon parse it once, the same way
//! every other client (a future TUI, an MCP tool) will. Reimplementing even
//! the "strip one leading character" part of that grammar here would be a
//! second parser for the same syntax, and the two are guaranteed to drift
//! (see `query::parse`'s own module docs on negation/sigil ordering — a
//! detail this file would otherwise have to keep in sync by hand).
//!
//! # Streaming: printed as it arrives, not collected
//!
//! `SearchService.Search`'s entire point (see that module's own docs) is
//! that the first hit is flushed before the rest of the page is computed.
//! [`search`] and [`similar`] both print each hit the moment it comes off
//! the stream — `while let Some(item) = stream.next().await { print_hit(...) }`
//! — and flush stdout after every one, rather than collecting into a `Vec`
//! and printing once the stream ends. Buffering here would silently throw
//! away the latency the pipeline was built to deliver.
//!
//! # The `--json` schema
//!
//! [`JsonHit`] is a hand-written type, not `#[derive(Serialize)]` on the
//! wire [`SearchHit`]/[`Snippet`]/[`RankExplanation`] (which don't derive
//! `serde::Serialize` at all — `rmail-proto`'s generated code carries no
//! serde attributes). That is deliberate, not an oversight: a proto field
//! rename is a wire-compatible, source-compatible change for every existing
//! gRPC client, but it would silently reshape this flag's output and break
//! every downstream `jq` expression the moment someone derived `Serialize`
//! directly on the generated struct. Naming the JSON fields explicitly, once,
//! here, is what makes `--json` an actual contract instead of an accident of
//! whatever the proto happens to derive.
//!
//! Field set (every hit, `search` and `similar` alike):
//!
//! ```json
//! {
//!   "uid": 4471,
//!   "subject": "Invoice #338 — Acme",
//!   "from": "billing@acme.com",
//!   "date": "2026-06-30T10:12:00Z",
//!   "score": 18.42,
//!   "snippet": {
//!     "text": "Your invoice for June is attached. Total $4,200 …",
//!     "highlights": [{ "start": 5, "end": 12 }]
//!   },
//!   "sources": ["lexical", "dense", "entity"],
//!   "thread_id": 88,
//!   "thread_collapsed": [],
//!   "near_duplicates": [],
//!   "why": null
//! }
//! ```
//!
//! `uid`/`subject`/`score`/`snippet`/`sources`/`why` are prd.md's own item
//! schema by name; `from`/`date` are prd.md's too, just not repeated in this
//! task's own acceptance bullet. `thread_id`/`thread_collapsed`/
//! `near_duplicates` are additive — real data `SearchHit` carries that
//! prd.md's older mockup predates — kept rather than dropped, since a
//! scripting consumer that does not want them can simply not read the key.
//!
//! Two shapes deliberately do **not** match prd.md's own illustration
//! literally:
//!
//! - **`snippet` is `{ text, highlights }`, never a string with markup
//!   spliced in.** `present::Snippet` already carries byte offsets instead
//!   of embedded delimiters for exactly this reason (see that module's own
//!   "Offsets, never embedded markup" doc section) — emitting anything else
//!   here would re-introduce the escaping/injection surface that design
//!   exists to avoid, and would force every consumer to write its own
//!   markup-stripping code to get the plain text back.
//! - **`why` is the structured `RankExplanation` (`score`, `sources`,
//!   `features[]`, `matched`, `claude_reason`), never a flattened
//!   human-readable string.** prd.md's mockup shows a joined string ("subject
//!   match • semantic match • …"); the real pipeline instead computes a
//!   per-feature contribution breakdown, which is strictly more useful to a
//!   script (sort by `weighted_contribution`, thresholds, etc.) and would
//!   only be *lost* by flattening it to text — the same "render your own,
//!   don't inherit mine" argument the snippet shape makes.
//!
//! Every hit carries the identical key set regardless of what actually
//! matched: `thread_id`/`why` are JSON `null` rather than an absent key when
//! there is nothing to report, and `thread_collapsed`/`near_duplicates` are
//! `[]` rather than omitted. A consistent key set means a consumer can write
//! one `jq`/`serde_json` shape for every line instead of branching on
//! whether a key happens to be present — task 42's global `--format json`
//! reuses this exact shape, so the fewer "is this key here" special cases a
//! consumer has to carry forward, the better.
//!
//! Output is one JSON object per line (newline-delimited, not a wrapping
//! `[ ... ]` array): the streaming property above only survives serialization
//! if each hit can be written the moment it arrives — a single JSON array
//! cannot be closed until the stream ends, which would put this flag back to
//! buffering everything, the exact thing streaming was supposed to avoid.
//!
//! # Terminal safety: highlights render as ANSI, never as unescaped bytes
//!
//! `--json` mode is safe by construction: `serde_json` escapes every
//! `U+0000..=U+001F` control character (RFC 8259 requires it), so an `ESC`
//! byte a hostile message body happens to contain comes out as the six
//! printable characters ``, never the real byte — a terminal
//! displaying that JSON text literally cannot be made to interpret it as an
//! escape sequence.
//!
//! The human-readable table has no such built-in protection — it writes
//! `Snippet::text` close to verbatim — so [`render_snippet`] sanitizes it
//! explicitly: every Unicode `Cc` control character (`char::is_control`) is
//! either normalized to a space (`\t`/`\r`/`\n`, so a snippet that happens to
//! contain one still reads as one line) or dropped outright (everything
//! else, `ESC` included — the byte that starts every ANSI/CSI/OSC sequence).
//! Dropping rather than substituting a visible placeholder is safe *because*
//! highlight ranges are matched against the *original* text's byte offsets
//! while this function walks it char-by-char and builds the output
//! incrementally — nothing downstream ever needs to translate an offset back
//! through the sanitized string, so removing bytes cannot desync a highlight
//! boundary from the character it should surround. Only `Cc` is treated this
//! way, not the broader family of Unicode characters that can also be used
//! to mislead a terminal (bidi overrides, zero-width joiners): those do not
//! *corrupt* the terminal the way a raw control byte can, and are a
//! different, wider problem than this task's acceptance criterion names.

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rmail_core::eval::{EvalReport, EvalThresholds, GoldenSet, Metrics, QueryEval};
use rmail_proto::v1::mail_service_client::MailServiceClient;
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::{
    ByteRange, CompileQueryRequest, EvalMetrics as WireEvalMetrics, EvalReport as WireEvalReport,
    EvaluateRequest, FeatureContribution, FullMessage, GetMessageRequest,
    GoldenQuery as WireGoldenQuery, Intent as ProtoIntent, Judgment as WireJudgment,
    Mode as ProtoMode, QueryPlan, RankExplanation, Rerank as ProtoRerank, SearchHit, SearchRequest,
    Snippet,
};
use tokio_stream::StreamExt;

/// `mail search <query>` flags — a near-1:1 mapping onto `SearchRequest`'s
/// own fields. The two exceptions: `--explore` sets `Intent::Exploratory`
/// rather than naming a `SearchRequest` field of its own (there is no
/// `explore` field on the wire — `explore` *is* the exploratory intent), and
/// `query`/`filter` are passed through unparsed (see the module docs).
#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct SearchArgs {
    /// `mail search eval` and friends. When absent, `search` is a plain
    /// ranked query over `query`.
    ///
    /// A subcommand name shadows that word as a query: `mail search eval`
    /// runs the harness rather than searching for "eval". Searching for the
    /// literal term is `mail search -- eval`. That trade is deliberate —
    /// prd.md and tasks.md both spell the verb `mail search eval`, and a
    /// one-word collision with an escape hatch is a smaller cost than
    /// inventing a different name for the command the spec names.
    #[command(subcommand)]
    action: Option<SearchAction>,
    /// Query text: free words, quoted phrases, `key:value` operators, and
    /// `~`/`=` sigils — parsed entirely server-side. Never inspected here.
    ///
    /// `allow_hyphen_values`: the grammar's own negation (`-tag:newsletter`,
    /// `-excludeterm`) starts with a hyphen, and clap would otherwise refuse
    /// `mail search -tag:newsletter` as an unrecognized flag rather than
    /// pass it through as query text — forcing every negated query onto the
    /// `mail search -- -tag:newsletter` escape hatch would make the CLI's
    /// hyphen behavior stricter than the grammar it is a client for.
    ///
    /// `Option` only because a subcommand replaces it (`subcommand_negates_reqs`);
    /// clap still requires it for a plain `mail search`.
    #[arg(allow_hyphen_values = true, required = true)]
    query: Option<String>,
    /// Additional operator-DSL text, space-joined onto `query` before the
    /// server parses it (`SearchRequest.filter`). Same `allow_hyphen_values`
    /// reasoning as `query`.
    #[arg(long, allow_hyphen_values = true)]
    filter: Option<String>,
    /// Restrict candidate generation to one retrieval strategy; omit to use
    /// the daemon's configured `search.default_mode`.
    #[arg(long, value_enum)]
    mode: Option<SearchModeArg>,
    /// Force exploratory intent: broader recall and MMR-diversified results
    /// instead of the classifier's own guess.
    #[arg(long)]
    explore: bool,
    /// Include the full ranking rationale (`RankExplanation`) on every hit.
    #[arg(long)]
    explain: bool,
    /// Emit one JSON object per hit (newline-delimited) instead of a
    /// human-readable table — see the module docs for the schema.
    #[arg(long)]
    json: bool,
    /// Results to return after ranking/diversification (0 = server
    /// default).
    #[arg(long)]
    limit: Option<u32>,
    /// Restrict to one account, by id.
    #[arg(long)]
    account: Option<i64>,
    /// Collapse each thread to its best-scoring message.
    #[arg(long = "thread-collapse")]
    thread_collapse: bool,
    /// Override `search.rerank` for this query: `off`, `cross-encoder`,
    /// `claude`, or `auto`. Reranking is always best-effort — a missing
    /// local model, a provider failure, or an exhausted AI budget quietly
    /// leaves the L1 ranking in place rather than failing the search.
    #[arg(long, value_enum)]
    rerank: Option<RerankArg>,
    /// Treat this as an explicit deep search: slower and more expensive is
    /// acceptable. Only `--rerank auto` (the usual default) reads it —
    /// that is what makes `auto` pick Claude rather than the local
    /// cross-encoder.
    #[arg(long)]
    deep: bool,
    /// Read `query` as plain English and have Claude compile it into a query
    /// first (`SearchService.CompileQuery`; prd.md Stage 0 step 7).
    ///
    /// The compiled plan is printed before anything runs — that is the
    /// "confirmable" half — and the search then uses it. `--plan-only` stops
    /// after printing. Compiles are cached per account by normalized
    /// question, so re-asking is free; `--refresh` recompiles.
    ///
    /// Requires `--account`: the plan cache and the AI budget that admits the
    /// call are both per account.
    #[arg(long, requires = "account")]
    nl: bool,
    /// Print the compiled plan and stop, without searching. Only meaningful
    /// with `--nl`.
    #[arg(long = "plan-only", requires = "nl")]
    plan_only: bool,
    /// Recompile instead of serving the cached plan. Only meaningful with
    /// `--nl`.
    #[arg(long, requires = "nl")]
    refresh: bool,
}

/// `mail similar <id>` flags. `similar` has no `--mode`/`--explore`/
/// `--filter`/`--account`: it always runs the dense-only `Semantic` RPC
/// against text derived from the named message's own content, not a
/// user-typed query, so those axes do not apply.
#[derive(Debug, clap::Args)]
pub struct SimilarArgs {
    /// Message id to find embedding-kNN neighbors of.
    id: i64,
    /// Neighbors to return. The source message itself never counts against
    /// this budget — see [`similar`]'s own doc comment.
    #[arg(long, default_value_t = 10)]
    limit: u32,
    /// Include the full ranking rationale on every neighbor.
    #[arg(long)]
    explain: bool,
    /// Emit one JSON object per neighbor (newline-delimited).
    #[arg(long)]
    json: bool,
}

/// Verbs that live under `mail search` rather than being a query.
#[derive(Debug, clap::Subcommand)]
enum SearchAction {
    /// Score a golden set against the local corpus and report NDCG@10, MRR,
    /// Recall@50 and P@3 (`SearchService.Evaluate`).
    Eval(EvalArgs),
}

/// `mail search eval` flags.
#[derive(Debug, clap::Args)]
pub struct EvalArgs {
    /// Path to the versioned golden-set TOML.
    #[arg(long, default_value = DEFAULT_GOLDEN_SET)]
    golden: PathBuf,
    /// Restrict candidate generation to one retrieval strategy; omit to
    /// evaluate the daemon's configured `search.default_mode` — which is
    /// what a regression guard should normally measure, since that is the
    /// configuration users actually get.
    #[arg(long, value_enum)]
    mode: Option<SearchModeArg>,
    /// Results to fetch per query. Clamped up to 50 server-side: Recall@50
    /// over a shorter page is unmeasurable rather than merely low.
    #[arg(long)]
    limit: Option<u32>,
    /// Fail (exit 1) if aggregate NDCG@10 falls below this.
    #[arg(long)]
    min_ndcg: Option<f64>,
    /// Fail if aggregate MRR falls below this.
    #[arg(long)]
    min_mrr: Option<f64>,
    /// Fail if aggregate Recall@50 falls below this.
    #[arg(long)]
    min_recall: Option<f64>,
    /// Fail if aggregate P@3 falls below this.
    #[arg(long)]
    min_p3: Option<f64>,
    /// Do not fail when a golden judgment names a message the corpus does
    /// not have. For a partially-synced developer mailbox that is expected;
    /// in CI against a seeded fixture it is a broken fixture, so gating runs
    /// treat it as a failure by default.
    #[arg(long)]
    allow_unresolved: bool,
    /// Emit the report as a single JSON object instead of a table.
    #[arg(long)]
    json: bool,
}

/// Where `mail search eval` looks for a golden set when `--golden` is
/// omitted. Repo-relative on purpose: the committed fixture set lives here,
/// so running the harness from a checkout needs no flags at all.
const DEFAULT_GOLDEN_SET: &str = "eval/golden.toml";

/// `SearchRequest.mode`, spelled the way a terminal user types it.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum SearchModeArg {
    Lexical,
    Semantic,
    Hybrid,
}

impl SearchModeArg {
    const fn into_proto(self) -> ProtoMode {
        match self {
            Self::Lexical => ProtoMode::Lexical,
            Self::Semantic => ProtoMode::Semantic,
            Self::Hybrid => ProtoMode::Hybrid,
        }
    }
}

/// `SearchRequest.rerank`, spelled the way a terminal user types it —
/// prd.md's `mail search "invoice" --rerank claude`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum RerankArg {
    Off,
    /// `cross-encoder` is what clap derives from the variant name; the
    /// `cross_encoder` alias is the spelling `search.rerank` and
    /// `RMAIL_SEARCH__RERANK` use, so the same word works everywhere.
    #[value(alias = "cross_encoder")]
    CrossEncoder,
    Claude,
    Auto,
}

impl RerankArg {
    const fn into_proto(self) -> ProtoRerank {
        match self {
            Self::Off => ProtoRerank::Off,
            Self::CrossEncoder => ProtoRerank::CrossEncoder,
            Self::Claude => ProtoRerank::Claude,
            Self::Auto => ProtoRerank::Auto,
        }
    }
}

/// `mail search <query>` — `SearchService.Search`, streamed straight to
/// stdout.
///
/// # Errors
///
/// Connection failure, an RPC-level error, or a mid-stream error status from
/// the daemon (surfaced as a plain [`anyhow::Error`] — the CLI has no
/// gRPC-status-aware caller to hand a typed error back to).
pub async fn search(socket: &Path, args: SearchArgs) -> Result<()> {
    if let Some(SearchAction::Eval(eval_args)) = args.action {
        return eval(socket, eval_args).await;
    }

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = SearchServiceClient::new(channel);

    // `required = true` on the arg; clap rejects a plain `mail search` with
    // no query before this runs, so the fallback is unreachable rather than a
    // silent empty-query search.
    let typed = args.query.unwrap_or_default();
    let query = if args.nl {
        let plan = client
            .compile_query(CompileQueryRequest {
                query: typed,
                account_id: args.account.unwrap_or(0),
                refresh: args.refresh,
            })
            .await
            .context("CompileQuery RPC failed")?
            .into_inner();
        // To stderr, not stdout: `mail search --nl ... --json | jq` must keep
        // emitting only hits. The plan is something a human reads before
        // deciding the answer was to the right question.
        print_plan(&plan);
        if args.plan_only {
            return Ok(());
        }
        plan.compiled
    } else {
        typed
    };

    let request = SearchRequest {
        query,
        filter: args.filter.unwrap_or_default(),
        mode: args
            .mode
            .map_or(ProtoMode::Unspecified, SearchModeArg::into_proto) as i32,
        intent: if args.explore {
            ProtoIntent::Exploratory as i32
        } else {
            ProtoIntent::Unspecified as i32
        },
        limit: args.limit.unwrap_or(0),
        explain: args.explain,
        thread_collapse: args.thread_collapse,
        account_id: args.account.unwrap_or(0),
        rerank: args
            .rerank
            .map_or(ProtoRerank::Unspecified, RerankArg::into_proto) as i32,
        deep: args.deep,
    };

    let mut stream = client
        .search(request)
        .await
        .context("Search RPC failed")?
        .into_inner();

    let styled = std::io::stdout().is_terminal();
    let mut shown = 0usize;
    while let Some(item) = stream.next().await {
        let hit = item.context("search stream item failed")?;
        print_hit(&hit, args.json, styled)?;
        shown += 1;
    }
    if shown == 0 && !args.json {
        println!("no results");
    }
    Ok(())
}

/// Print a compiled plan for a human to confirm, on stderr — see [`search`].
///
/// Prints what the daemon derived (`filters` re-parsed from the compiled
/// query), never a re-derivation of its own: a client that parsed the query
/// itself to describe it would be a second reading of the grammar, and the
/// one shown would be the one that is not enforced.
fn print_plan(plan: &QueryPlan) {
    let source = if plan.cached {
        "cached"
    } else {
        plan.model.as_str()
    };
    eprintln!("compiled ({source}): {}", plan.compiled);
    if !plan.filters.is_empty() {
        eprintln!("  filters:  {}", plan.filters.join(" "));
    }
    if !plan.semantic_query.is_empty() {
        eprintln!("  ranked:   {}", plan.semantic_query);
    }
    if !plan.notes.is_empty() {
        eprintln!("  reading:  {}", plan.notes);
    }
}

/// `mail search eval` — score the golden set and, when asked to gate, fail
/// the process on a regression (prd.md: "Relevance is measured, not
/// asserted"; task 37).
///
/// The golden-set file is parsed and validated **here**, client-side, before
/// anything is sent: `rmail_core::eval::GoldenSet` is a shared type, so the
/// daemon would reject the same violations anyway, and catching a typo'd
/// TOML file locally gives a message about a path the user can see rather
/// than an `INVALID_ARGUMENT` about a request they did not hand-write.
///
/// # Gating
///
/// With no `--min-*` flag this reports and exits 0 — the mode for a
/// developer reading numbers. Passing any threshold turns it into a gate:
/// every threshold is checked, unresolved judgments count as a failure
/// unless `--allow-unresolved`, and a violation exits non-zero so CI stops.
///
/// # Errors
///
/// A missing or malformed golden set, connection failure, an `Evaluate` RPC
/// error, or — when gating — a threshold violation.
async fn eval(socket: &Path, args: EvalArgs) -> Result<()> {
    let set = GoldenSet::load(&args.golden)
        .with_context(|| format!("loading golden set {}", args.golden.display()))?;

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = SearchServiceClient::new(channel);

    let request = EvaluateRequest {
        corpus: set.corpus.clone(),
        queries: set
            .queries
            .iter()
            .map(|q| WireGoldenQuery {
                name: q.name.clone(),
                query: q.query.clone(),
                account_id: q.account_id,
                judgments: q
                    .judgments
                    .iter()
                    .map(|j| WireJudgment {
                        message_id: j.message_id.clone(),
                        gain: j.gain,
                    })
                    .collect(),
            })
            .collect(),
        mode: args
            .mode
            .map_or(ProtoMode::Unspecified, SearchModeArg::into_proto) as i32,
        limit: args.limit.unwrap_or(0),
    };

    let report = client
        .evaluate(request)
        .await
        .context("Evaluate RPC failed")?
        .into_inner();

    if args.json {
        print_eval_json(&report)?;
    } else {
        print_eval_table(&report);
    }

    let gating = args.min_ndcg.is_some()
        || args.min_mrr.is_some()
        || args.min_recall.is_some()
        || args.min_p3.is_some();
    if !gating {
        // Not a gate, but an unresolved judgment still means the numbers
        // just printed understate the ranker — worth saying out loud rather
        // than leaving someone to wonder why NDCG looks low.
        let unresolved: Vec<&str> = report
            .per_query
            .iter()
            .flat_map(|q| q.unresolved.iter().map(String::as_str))
            .collect();
        if !unresolved.is_empty() {
            eprintln!(
                "warning: {} judged message(s) are not in this corpus, so these \
                 metrics understate the ranker: {}",
                unresolved.len(),
                unresolved.join(", ")
            );
        }
        return Ok(());
    }

    let core = to_core_report(&report);
    let thresholds = EvalThresholds {
        min_ndcg_at_10: args.min_ndcg.unwrap_or(0.0),
        min_mrr: args.min_mrr,
        min_recall_at_50: args.min_recall,
        min_p_at_3: args.min_p3,
        require_resolved: !args.allow_unresolved,
    };

    match thresholds.check(&core) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Print the worst queries alongside the failure: the aggregate
            // says a regression happened, and these say where to look.
            eprintln!("\nworst queries by NDCG@10:");
            for q in core.worst(5) {
                eprintln!(
                    "  {:<28} ndcg@10={:.4} mrr={:.4} returned={} relevant={}",
                    q.name, q.metrics.ndcg_at_10, q.metrics.mrr, q.returned, q.relevant
                );
            }
            Err(anyhow!("{error}"))
        }
    }
}

/// Rebuild the core report from the wire one so the *same*
/// `EvalThresholds::check` that gates a test also gates the CLI — a second
/// threshold implementation here could disagree with the first, and the one
/// place that must never happen is the code that decides whether to fail a
/// build.
fn to_core_report(report: &WireEvalReport) -> EvalReport {
    let metrics_of = |m: Option<&WireEvalMetrics>| Metrics {
        ndcg_at_10: m.map_or(0.0, |m| m.ndcg_at_10),
        mrr: m.map_or(0.0, |m| m.mrr),
        recall_at_50: m.map_or(0.0, |m| m.recall_at_50),
        p_at_3: m.map_or(0.0, |m| m.p_at_3),
    };
    EvalReport {
        corpus: report.corpus.clone(),
        aggregate: metrics_of(report.aggregate.as_ref()),
        per_query: report
            .per_query
            .iter()
            .map(|q| QueryEval {
                name: q.name.clone(),
                query: q.query.clone(),
                metrics: metrics_of(q.metrics.as_ref()),
                returned: q.returned as usize,
                relevant: q.relevant as usize,
                unresolved: q.unresolved.clone(),
            })
            .collect(),
    }
}

fn print_eval_table(report: &WireEvalReport) {
    println!("corpus: {}", report.corpus);
    println!(
        "\n{:<28} {:>9} {:>9} {:>11} {:>7} {:>9}",
        "query", "ndcg@10", "mrr", "recall@50", "p@3", "returned"
    );
    for q in &report.per_query {
        let m = q.metrics.unwrap_or_default();
        println!(
            "{:<28} {:>9.4} {:>9.4} {:>11.4} {:>7.4} {:>9}",
            truncate(&q.name, 28),
            m.ndcg_at_10,
            m.mrr,
            m.recall_at_50,
            m.p_at_3,
            q.returned
        );
        if !q.unresolved.is_empty() {
            println!("  ! not in corpus: {}", q.unresolved.join(", "));
        }
    }

    let agg = report.aggregate.unwrap_or_default();
    println!(
        "\n{:<28} {:>9.4} {:>9.4} {:>11.4} {:>7.4}",
        format!("AGGREGATE ({} queries)", report.per_query.len()),
        agg.ndcg_at_10,
        agg.mrr,
        agg.recall_at_50,
        agg.p_at_3
    );
}

/// One JSON object for the whole report — unlike `mail search`, which emits
/// newline-delimited per-hit objects. A report is a single value with a
/// single aggregate, and splitting it across lines would make a consumer
/// reassemble something that was never a stream.
fn print_eval_json(report: &WireEvalReport) -> Result<()> {
    let value = serde_json::json!({
        "corpus": report.corpus,
        "aggregate": metrics_json(report.aggregate.as_ref()),
        "per_query": report
            .per_query
            .iter()
            .map(|q| serde_json::json!({
                "name": q.name,
                "query": q.query,
                "metrics": metrics_json(q.metrics.as_ref()),
                "returned": q.returned,
                "relevant": q.relevant,
                "unresolved": q.unresolved,
            }))
            .collect::<Vec<_>>(),
    });
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &value).context("serializing eval report")?;
    writeln!(out).context("writing eval report")?;
    Ok(())
}

fn metrics_json(m: Option<&WireEvalMetrics>) -> serde_json::Value {
    let m = m.copied().unwrap_or_default();
    serde_json::json!({
        "ndcg_at_10": m.ndcg_at_10,
        "mrr": m.mrr,
        "recall_at_50": m.recall_at_50,
        "p_at_3": m.p_at_3,
    })
}

/// Clip a name to `max` characters so one long golden-query name cannot
/// shear the whole table's columns.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// `mail similar <id>` — the embedding-kNN neighbors of an already-indexed
/// message (prd.md: "embedding kNN neighbors of a message").
///
/// There is no dedicated "neighbors of a message" RPC; this composes two
/// existing ones, exactly as a thin gRPC client is supposed to: `MailService
/// .Get` fetches the message's own subject/body, which becomes the query
/// text for a dense-only `SearchService.Semantic` call. The source message
/// almost always comes back as `Semantic`'s own top hit (nothing embeds
/// closer to a message's vector than the message itself), so it is filtered
/// out of the printed results rather than counted as its own neighbor —
/// `limit + 1` candidates are requested from the server for exactly this
/// reason, so filtering the one self-match still leaves a full page.
///
/// # Errors
///
/// Connection failure, `Get` returning `NOT_FOUND` (bad id), a message with
/// neither a subject nor a body to build a query from, or a `Semantic`
/// RPC/stream error.
pub async fn similar(socket: &Path, args: SimilarArgs) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut mail_client = MailServiceClient::new(channel.clone());
    let mut search_client = SearchServiceClient::new(channel);

    let full = mail_client
        .get(GetMessageRequest { id: args.id })
        .await
        .with_context(|| format!("Get RPC failed for message {}", args.id))?
        .into_inner();
    let query = similar_query_text(&full).ok_or_else(|| {
        anyhow!(
            "message {} has neither a subject nor a body to compare against",
            args.id
        )
    })?;

    let request = SearchRequest {
        query,
        limit: args.limit.saturating_add(1),
        explain: args.explain,
        ..Default::default()
    };
    let mut stream = search_client
        .semantic(request)
        .await
        .context("Semantic RPC failed")?
        .into_inner();

    let styled = std::io::stdout().is_terminal();
    let mut shown = 0u32;
    while shown < args.limit {
        let Some(item) = stream.next().await else {
            break;
        };
        let hit = item.context("semantic stream item failed")?;
        if hit.message.as_ref().is_some_and(|m| m.id == args.id) {
            continue; // the source message is not a neighbor of itself.
        }
        print_hit(&hit, args.json, styled)?;
        shown += 1;
    }
    if shown == 0 && !args.json {
        println!("no similar messages found");
    }
    Ok(())
}

/// How much of a message's own subject+body seeds a `similar` query.
/// Generous relative to what a query embedder needs to fix a topic (a
/// handful of sentences already does), capped so one very long message body
/// never dominates the request — the same order of magnitude as
/// `present::snippet::MAX_SOURCE_CHARS` in rmail-core (not reused directly:
/// it is a private module constant, and this crate has no dependency on
/// `rmail-core`'s internals beyond its public API).
const SIMILAR_QUERY_MAX_CHARS: usize = 2_000;

/// Build a `similar` query from a fetched message: subject and (truncated)
/// body, space-joined. `None` only when both are empty, which
/// [`similar`] treats as "nothing to compare against" rather than silently
/// issuing an empty-string semantic search (which the daemon would have to
/// special-case, and which would not mean anything sensible anyway — the
/// dense retriever finds a message's *nearest* neighbors, not "all
/// messages").
fn similar_query_text(full: &FullMessage) -> Option<String> {
    let subject = full
        .message
        .as_ref()
        .and_then(|m| m.subject.clone())
        .filter(|s| !s.trim().is_empty());
    let body = full
        .body_text
        .as_deref()
        .map(|b| truncate_chars(b, SIMILAR_QUERY_MAX_CHARS))
        .filter(|b| !b.trim().is_empty());

    match (subject, body) {
        (Some(s), Some(b)) => Some(format!("{s} {b}")),
        (Some(s), None) => Some(s),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// `text`, truncated to at most `max_chars` **characters** (not bytes) —
/// snapped to a char boundary so the cap can never split a multi-byte
/// character.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte_at, _)) => text[..byte_at].to_owned(),
        None => text.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Output: one hit at a time, JSON or human-readable
// ---------------------------------------------------------------------------

/// Print one hit and flush immediately — see the module docs' "Streaming"
/// section for why this cannot batch.
fn print_hit(hit: &SearchHit, json: bool, styled: bool) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if json {
        let line = serde_json::to_string(&JsonHit::from_wire(hit))
            .context("failed to serialize a search hit as JSON")?;
        writeln!(out, "{line}").context("failed to write search output")?;
    } else {
        render_human(&mut out, hit, styled).context("failed to write search output")?;
    }
    out.flush().context("failed to flush stdout")?;
    Ok(())
}

fn render_human(
    out: &mut impl std::io::Write,
    hit: &SearchHit,
    styled: bool,
) -> std::io::Result<()> {
    let message = hit.message.as_ref();
    let uid = message.map_or(0, |m| m.id);
    let subject = message
        .and_then(|m| m.subject.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or("(no subject)");
    let from = message
        .and_then(|m| m.from_addr.clone().or_else(|| m.from_name.clone()))
        .unwrap_or_else(|| "(unknown sender)".to_owned());
    let date = message
        .and_then(|m| m.date)
        .and_then(format_rfc3339)
        .unwrap_or_else(|| "-".to_owned());

    writeln!(out, "{:>7.2}  {subject}", hit.score)?;
    writeln!(
        out,
        "         #{uid}  {from}  {date}  [{}]",
        hit.sources.join(", ")
    )?;
    if let Some(snippet) = &hit.snippet {
        writeln!(out, "         {}", render_snippet(snippet, styled))?;
    }
    if hit.thread_id.is_some()
        || !hit.thread_collapsed.is_empty()
        || !hit.near_duplicates.is_empty()
    {
        let mut parts = Vec::new();
        if let Some(thread_id) = hit.thread_id {
            parts.push(format!("thread {thread_id}"));
        }
        if !hit.thread_collapsed.is_empty() {
            parts.push(format!("+{} collapsed", hit.thread_collapsed.len()));
        }
        if !hit.near_duplicates.is_empty() {
            parts.push(format!("{} near-dup", hit.near_duplicates.len()));
        }
        writeln!(out, "         {}", parts.join(", "))?;
    }
    if let Some(why) = &hit.why {
        render_explanation(out, why)?;
    }
    writeln!(out)
}

fn render_explanation(out: &mut impl std::io::Write, why: &RankExplanation) -> std::io::Result<()> {
    writeln!(out, "         why (score {:.3}):", why.score)?;
    for feature in &why.features {
        writeln!(
            out,
            "           {:<24} value={:>8.3} weight={:>6.3} -> {:>8.3}",
            feature.name, feature.value, feature.weight, feature.weighted_contribution
        )?;
    }
    if !why.claude_reason.is_empty() {
        writeln!(out, "           claude: {}", why.claude_reason)?;
    }
    Ok(())
}

/// ANSI SGR codes bracketing a highlighted span: bold, reasonably legible on
/// both light and dark terminal themes without committing to a color (a
/// color choice risks clashing with either theme; bold does not).
const HIGHLIGHT_ON: &str = "\x1b[1m";
const HIGHLIGHT_OFF: &str = "\x1b[0m";

/// Render a [`Snippet`] for the terminal: apply `highlights` as ANSI bold
/// when `styled` (stdout is a tty), and always sanitize control characters —
/// see the module docs' "Terminal safety" section. `styled` is checked once
/// by the caller ([`print_hit`]) rather than here so this function stays a
/// pure `Snippet -> String` transform, easy to unit test without touching
/// stdout.
fn render_snippet(snippet: &Snippet, styled: bool) -> String {
    let ranges = valid_ranges(snippet);
    let mut out = String::with_capacity(snippet.text.len());
    // Emit the SGR codes on the *transitions* into and out of a highlight,
    // not around each character. Per-character bracketing renders the same
    // and is far easier to write, but a seven-character match becomes seven
    // on/off pairs — 56 bytes of escapes around 7 bytes of text — which
    // bloats every line, defeats a terminal's own run-length handling, and
    // makes the output painful to read when piped somewhere that shows the
    // escapes literally.
    let mut open = false;
    for (idx, ch) in snippet.text.char_indices() {
        let in_highlight = ranges.iter().any(|&(start, end)| idx >= start && idx < end);
        if styled && in_highlight != open {
            out.push_str(if in_highlight {
                HIGHLIGHT_ON
            } else {
                HIGHLIGHT_OFF
            });
            open = in_highlight;
        }
        push_sanitized(&mut out, ch);
    }
    // A highlight running to the end of the text still has to be closed, or
    // the escape leaks into whatever the terminal prints next.
    if open {
        out.push_str(HIGHLIGHT_OFF);
    }
    out
}

/// `snippet.highlights`, decoded to `usize` and kept only when they are a
/// valid, non-empty, char-boundary-respecting slice of `snippet.text`.
/// `Snippet`'s own contract (see `present::snippet`'s doc comment) already
/// guarantees this from the daemon, but this value crossed a wire the CLI
/// does not otherwise trust — a malformed or adversarial range must degrade
/// to "not highlighted," never panic a slicing operation downstream.
fn valid_ranges(snippet: &Snippet) -> Vec<(usize, usize)> {
    snippet
        .highlights
        .iter()
        .filter_map(|r: &ByteRange| {
            let start = usize::try_from(r.start).ok()?;
            let end = usize::try_from(r.end).ok()?;
            let text = &snippet.text;
            (start < end
                && end <= text.len()
                && text.is_char_boundary(start)
                && text.is_char_boundary(end))
            .then_some((start, end))
        })
        .collect()
}

/// Append `ch` to `out`, neutralizing anything that could issue a terminal
/// escape sequence or other control effect. See the module docs' "Terminal
/// safety" section for why dropping (rather than substituting a visible
/// placeholder) is safe here.
fn push_sanitized(out: &mut String, ch: char) {
    match ch {
        '\n' | '\r' | '\t' => out.push(' '),
        c if c.is_control() => {}
        c => out.push(c),
    }
}

fn format_rfc3339(unix_seconds: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(unix_seconds, 0)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
}

// ---------------------------------------------------------------------------
// The `--json` contract
// ---------------------------------------------------------------------------

/// One `--json` output line. See the module docs for the full schema and
/// why its shape deliberately does not match prd.md's illustration
/// literally in two places (`snippet`, `why`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct JsonHit {
    uid: i64,
    subject: Option<String>,
    from: Option<String>,
    date: Option<String>,
    score: f64,
    snippet: JsonSnippet,
    sources: Vec<String>,
    thread_id: Option<i64>,
    thread_collapsed: Vec<i64>,
    near_duplicates: Vec<i64>,
    why: Option<JsonExplanation>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
struct JsonSnippet {
    text: String,
    highlights: Vec<JsonRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
struct JsonRange {
    start: u32,
    end: u32,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct JsonExplanation {
    score: f64,
    sources: Vec<String>,
    features: Vec<JsonFeature>,
    matched: Option<JsonSnippet>,
    claude_reason: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
struct JsonFeature {
    name: String,
    value: f64,
    weight: f64,
    weighted_contribution: f64,
}

impl JsonHit {
    fn from_wire(hit: &SearchHit) -> Self {
        let message = hit.message.as_ref();
        Self {
            uid: message.map_or(0, |m| m.id),
            subject: message.and_then(|m| m.subject.clone()),
            from: message.and_then(|m| m.from_addr.clone().or_else(|| m.from_name.clone())),
            date: message.and_then(|m| m.date).and_then(format_rfc3339),
            score: hit.score,
            snippet: hit
                .snippet
                .as_ref()
                .map(JsonSnippet::from_wire)
                .unwrap_or_default(),
            sources: hit.sources.clone(),
            thread_id: hit.thread_id,
            thread_collapsed: hit.thread_collapsed.clone(),
            near_duplicates: hit.near_duplicates.clone(),
            why: hit.why.as_ref().map(JsonExplanation::from_wire),
        }
    }
}

impl JsonSnippet {
    fn from_wire(snippet: &Snippet) -> Self {
        Self {
            text: snippet.text.clone(),
            highlights: snippet
                .highlights
                .iter()
                .map(|r| JsonRange {
                    start: r.start,
                    end: r.end,
                })
                .collect(),
        }
    }
}

impl JsonExplanation {
    fn from_wire(why: &RankExplanation) -> Self {
        Self {
            score: why.score,
            sources: why.sources.clone(),
            features: why.features.iter().map(JsonFeature::from_wire).collect(),
            matched: why.matched.as_ref().map(JsonSnippet::from_wire),
            claude_reason: why.claude_reason.clone(),
        }
    }
}

impl JsonFeature {
    fn from_wire(f: &FeatureContribution) -> Self {
        Self {
            name: f.name.clone(),
            value: f.value,
            weight: f.weight,
            weighted_contribution: f.weighted_contribution,
        }
    }
}

#[cfg(test)]
mod tests;
