//! Integration test: `SearchService.Evaluate` end-to-end, and the relevance
//! regression guard itself (task 37).
//!
//! This file is the CI gate prd.md asks for ("CI runs the golden set; a drop
//! in NDCG fails the build"). It is a test rather than a bespoke workflow
//! step because a test is hermetic — it seeds its own corpus, runs in the
//! same container the rest of the suite does, and needs no daemon, no
//! network, and no developer's mailbox — while still failing the build on a
//! regression, which is the entire requirement. `.github/workflows/ci.yml`
//! additionally runs it as its own named step so a relevance drop is legible
//! in the CI UI rather than buried in a 1600-test summary.
//!
//! # The corpus and the golden set are checked against each other
//!
//! [`FIXTURE`] seeds the messages `eval/golden.toml` judges, and the golden
//! set is **loaded from that committed file** rather than built inline. That
//! coupling is deliberate in both directions: a judgment naming a message
//! the fixture does not seed fails the run (an unresolved judgment is a hard
//! error, not a silent zero), and a fixture message nobody judges is simply
//! unjudged noise, which is exactly what a corpus should have. The file
//! cannot rot unnoticed, because the file is what runs.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::eval::{EvalReport, EvalThresholds, GoldenSet, Metrics, QueryEval};
use rmail_core::index::fts::FtsIndex;
use rmail_core::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use rmail_core::repo;
use rmail_core::{Config, Database};
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::{
    EvalReport as WireEvalReport, EvaluateRequest, GoldenQuery as WireGoldenQuery,
    Judgment as WireJudgment, Mode as ProtoMode,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The floor the shipped ranker must clear on the fixture corpus.
///
/// The pipeline currently scores **0.9723** here. The floor sits below that,
/// not at it: a threshold pinned to today's exact number turns every harmless
/// retuning into a red build, and a gate that cries wolf is a gate people
/// learn to override.
///
/// It is placed by arithmetic rather than by feel. With six golden queries,
/// one of them losing its relevant results entirely drops the aggregate to
/// `(0.9723 * 6 - 1.0) / 6 ≈ 0.806`, so a floor above that catches a whole
/// query regressing — the coarsest failure worth catching — while still
/// leaving ~0.12 of headroom for retuning that shuffles results without
/// losing them. Raise it deliberately as the pipeline improves; that is how
/// a ratchet works.
const NDCG_FLOOR: f64 = 0.85;

/// Floors for the supporting metrics, both currently at 1.0.
///
/// MRR tracks the navigational cases prd.md's "top 3" criterion is about; at
/// 0.80, one query's best hit falling from rank 1 to rank 3 (aggregate
/// `≈ 0.889`) passes, while two doing so (`≈ 0.778`) fails. Recall is the
/// metric a candidate-generation change moves first — below 0.9 a judged
/// message has fallen out of the top 50 entirely, which is a recall bug
/// rather than a ranking one and should be loud.
const MRR_FLOOR: f64 = 0.80;
const RECALL_FLOOR: f64 = 0.9;

/// The fixture corpus: `(Message-ID, from, subject, body)`.
///
/// Deliberately more than the golden set judges. A corpus where every
/// message is a correct answer to something cannot distinguish a ranker from
/// a shuffler — the unjudged messages here (standup notes, PTO, the Stripe
/// and Acme invoices) exist to be plausible-but-wrong answers that a weak
/// ranker will surface and a good one will not. The two invoices in
/// particular are near-misses for `aws-invoice`: they share the strongest
/// term in the query.
const FIXTURE: &[(&str, &str, &str, &str)] = &[
    (
        "<aws-jul@example.com>",
        "billing@aws.amazon.com",
        "AWS invoice for July",
        "Your AWS invoice for July is ready. EC2 and S3 charges for the billing \
         period total 412.55 USD. View the full invoice in the billing console.",
    ),
    (
        "<aws-aug@example.com>",
        "billing@aws.amazon.com",
        "AWS invoice for August",
        "Your AWS invoice for August is ready. EC2, S3 and CloudFront charges \
         for the billing period total 508.10 USD.",
    ),
    (
        "<stripe-jul@example.com>",
        "receipts@stripe.com",
        "Stripe payout summary",
        "Your payout of 2,140.00 USD has been sent to your bank account. This \
         summary covers charges settled during the period.",
    ),
    (
        "<invoice-acme@example.com>",
        "ap@acme.example",
        "Acme consulting invoice 2291",
        "Please find attached invoice 2291 for consulting services rendered. \
         Payment terms are net 30 from the invoice date.",
    ),
    (
        "<office-move@example.com>",
        "alice@corp.example",
        "Office move logistics",
        "We are moving to the new office on the 14th. Pack your desk the night \
         before; the movers arrive at 8am and the lifts are booked all morning.",
    ),
    (
        "<office-move-2@example.com>",
        "bob@corp.example",
        "Re: Office move logistics",
        "Following up on the move — can we get more boxes for the hardware lab? \
         The current allocation will not cover the test rigs.",
    ),
    (
        "<standup@example.com>",
        "bob@corp.example",
        "Daily standup notes",
        "Yesterday: finished the retry path. Today: connection pooling. \
         Blockers: none.",
    ),
    (
        "<newsletter-1@example.com>",
        "editor@rustweekly.example",
        "Rust Weekly #500",
        "This week in Rust: async trait stabilization, a new profiler, and \
         three crates worth your attention.",
    ),
    (
        "<newsletter-2@example.com>",
        "editor@rustweekly.example",
        "Rust Weekly #501",
        "This week in Rust: const generics progress, an allocator deep dive, \
         and the quarterly survey results.",
    ),
    (
        "<offer@example.com>",
        "hr@corp.example",
        "Your job offer",
        "We are delighted to extend a job offer for the Staff Engineer role. \
         The full compensation details are enclosed; please respond by Friday.",
    ),
    (
        "<pto@example.com>",
        "hr@corp.example",
        "PTO approval",
        "Your time off request for the last week of August has been approved.",
    ),
    (
        "<security@example.com>",
        "no-reply@accounts.example",
        "Security alert: new sign-in",
        "We detected a new sign-in to your account from an unrecognized device \
         in Lisbon. If this was not you, secure your account immediately.",
    ),
];

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        let mut config = Config::default();
        // Same convention as `rmaild/tests/search_service.rs`: the
        // deterministic hash fallback keeps this suite from loading — or, on
        // a cold cache, downloading — an ONNX model. The golden queries here
        // are lexical/operator cases scored against the lexical and
        // structural retrievers; dense retrieval contributing nothing is a
        // *harder* test of the rest of the pipeline, not an easier one.
        config.index.semantic.enabled = false;

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-eval-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-eval-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let db = Database::open(&db_path).unwrap();

        let (account_id, mailbox_id) = db
            .with_write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: format!("Personal-{n}"),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .unwrap();

        let fts = FtsIndex::new(db.clone(), config.search.bm25_weights.clone());
        let queue = IndexQueue::new(db.clone(), QueueOptions::default());

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds_with_config(&server_socket, server_db, config, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut ready = false;
        for _ in 0..200 {
            if rmail_core::connect_uds(&socket).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "server never became ready");

        let server = Self {
            socket,
            db_path,
            db,
            fts,
            queue,
            account_id,
            mailbox_id,
            shutdown: shutdown_tx,
            handle,
        };
        server.seed().await;
        server
    }

    /// Insert, extract and index every fixture message — the real pipeline,
    /// same as `search_service.rs`'s own `index` helper.
    async fn seed(&self) {
        for (uid, (message_id, from, subject, body)) in FIXTURE.iter().enumerate() {
            let new = repo::NewMessage {
                account_id: self.account_id,
                mailbox_id: self.mailbox_id,
                uid: uid as i64 + 1,
                uidvalidity: 1,
                message_id: Some((*message_id).to_owned()),
                subject: Some((*subject).to_owned()),
                from_addr: Some((*from).to_owned()),
                from_name: Some(from.split('@').next().unwrap_or(from).to_owned()),
                body_text: Some((*body).to_owned()),
                date: Some(1_700_000_000 + uid as i64 * 3600),
                ..Default::default()
            };
            let id = self
                .db
                .with_write(move |c| repo::insert_message(c, &new))
                .unwrap();
            extract_message(&self.db, &self.queue, id, PRIORITY_NORMAL)
                .await
                .unwrap();
            self.fts.index_message(id).await.unwrap();
        }
    }

    async fn client(&self) -> SearchServiceClient<Channel> {
        SearchServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// The committed golden set — loaded from `eval/golden.toml` so the file that
/// ships is the file that runs.
fn golden_set() -> GoldenSet {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("eval")
        .join("golden.toml");
    GoldenSet::load(&path).unwrap_or_else(|e| panic!("loading {}: {e}", path.display()))
}

fn to_request(set: &GoldenSet, mode: ProtoMode) -> EvaluateRequest {
    EvaluateRequest {
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
        mode: mode as i32,
        limit: 0,
    }
}

/// Rebuild the core report from the wire one so the threshold check in this
/// test is the same `EvalThresholds::check` the CLI gates on.
fn to_core(report: &WireEvalReport) -> EvalReport {
    let metrics_of = |m: Option<&rmail_proto::v1::EvalMetrics>| Metrics {
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

// ---------------------------------------------------------------------------
// The regression guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_shipped_ranker_clears_the_relevance_floor_on_the_golden_set() {
    let server = TestServer::start().await;
    let set = golden_set();
    let report = server
        .client()
        .await
        .evaluate(to_request(&set, ProtoMode::Unspecified))
        .await
        .expect("Evaluate")
        .into_inner();

    let core = to_core(&report);

    // Printed unconditionally: when this fails in CI, the numbers and the
    // per-query breakdown need to be in the log already, not one rerun away.
    println!("corpus: {}", core.corpus);
    for q in &core.per_query {
        println!(
            "  {:<20} ndcg@10={:.4} mrr={:.4} recall@50={:.4} p@3={:.4} returned={} relevant={}",
            q.name,
            q.metrics.ndcg_at_10,
            q.metrics.mrr,
            q.metrics.recall_at_50,
            q.metrics.p_at_3,
            q.returned,
            q.relevant
        );
    }
    println!(
        "  AGGREGATE ndcg@10={:.4} mrr={:.4} recall@50={:.4} p@3={:.4}",
        core.aggregate.ndcg_at_10,
        core.aggregate.mrr,
        core.aggregate.recall_at_50,
        core.aggregate.p_at_3
    );

    let verdict = EvalThresholds {
        min_ndcg_at_10: NDCG_FLOOR,
        min_mrr: Some(MRR_FLOOR),
        min_recall_at_50: Some(RECALL_FLOOR),
        min_p_at_3: None,
        require_resolved: true,
    }
    .check(&core);

    server.stop().await;
    verdict.expect("relevance regression");
}

#[tokio::test]
async fn every_golden_judgment_resolves_against_the_fixture_corpus() {
    // The guard above would also catch this, but conflated with a relevance
    // failure. Isolating it means a fixture/golden-set mismatch names itself
    // instead of arriving disguised as a ranking regression.
    let server = TestServer::start().await;
    let set = golden_set();
    let report = server
        .client()
        .await
        .evaluate(to_request(&set, ProtoMode::Unspecified))
        .await
        .expect("Evaluate")
        .into_inner();

    let unresolved: Vec<&str> = report
        .per_query
        .iter()
        .flat_map(|q| q.unresolved.iter().map(String::as_str))
        .collect();
    let missing = unresolved.join(", ");
    server.stop().await;

    assert!(
        unresolved.is_empty(),
        "eval/golden.toml judges message(s) the fixture corpus does not seed: {missing}"
    );
}

// ---------------------------------------------------------------------------
// The RPC surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_report_mirrors_the_request_query_for_query() {
    let server = TestServer::start().await;
    let set = golden_set();
    let report = server
        .client()
        .await
        .evaluate(to_request(&set, ProtoMode::Unspecified))
        .await
        .expect("Evaluate")
        .into_inner();

    assert_eq!(report.corpus, set.corpus);
    assert_eq!(report.per_query.len(), set.queries.len());
    for (got, want) in report.per_query.iter().zip(&set.queries) {
        assert_eq!(got.name, want.name, "reports must stay in request order");
        assert_eq!(got.query, want.query);
        assert!(got.metrics.is_some(), "every query carries its metrics");
    }
    assert!(report.aggregate.is_some());

    server.stop().await;
}

#[tokio::test]
async fn the_aggregate_is_the_macro_average_of_the_per_query_metrics() {
    // Guards the one number a CI gate reads. If the aggregate ever stopped
    // being the mean of what it reports per query, the gate would be
    // measuring something nobody could reproduce from the printed rows.
    let server = TestServer::start().await;
    let report = server
        .client()
        .await
        .evaluate(to_request(&golden_set(), ProtoMode::Unspecified))
        .await
        .expect("Evaluate")
        .into_inner();
    server.stop().await;

    let n = report.per_query.len() as f64;
    let expected: f64 = report
        .per_query
        .iter()
        .filter_map(|q| q.metrics)
        .map(|m| m.ndcg_at_10)
        .sum::<f64>()
        / n;
    let aggregate = report.aggregate.expect("aggregate").ndcg_at_10;
    assert!(
        (aggregate - expected).abs() < 1e-9,
        "aggregate {aggregate} is not the mean {expected} of the per-query rows"
    );
}

#[tokio::test]
async fn an_unknown_message_id_is_reported_rather_than_scored_as_a_miss() {
    let server = TestServer::start().await;
    let request = EvaluateRequest {
        corpus: "ad-hoc".to_owned(),
        queries: vec![WireGoldenQuery {
            name: "q".to_owned(),
            query: "aws invoice".to_owned(),
            account_id: 0,
            judgments: vec![
                WireJudgment {
                    message_id: "<aws-jul@example.com>".to_owned(),
                    gain: 3,
                },
                WireJudgment {
                    message_id: "<not-in-this-corpus@example.com>".to_owned(),
                    gain: 3,
                },
            ],
        }],
        mode: ProtoMode::Unspecified as i32,
        limit: 0,
    };

    let report = server
        .client()
        .await
        .evaluate(request)
        .await
        .expect("Evaluate")
        .into_inner();
    server.stop().await;

    let q = &report.per_query[0];
    assert_eq!(q.unresolved, vec!["<not-in-this-corpus@example.com>"]);
    assert_eq!(q.relevant, 1, "only the resolvable judgment counts");
    // The resolvable one is still found, so this is visibly a corpus
    // problem and not a ranking one.
    assert!(q.metrics.expect("metrics").ndcg_at_10 > 0.0);
}

#[tokio::test]
async fn an_absent_gain_is_read_as_relevant_not_as_irrelevant() {
    // proto3 cannot distinguish an unset scalar from a zero one, so a golden
    // set that omits `gain` must not silently become a set with no relevant
    // messages — which would make every NDCG zero and the gate meaningless.
    let server = TestServer::start().await;
    let request = EvaluateRequest {
        corpus: "ad-hoc".to_owned(),
        queries: vec![WireGoldenQuery {
            name: "ungraded".to_owned(),
            query: "security alert sign-in".to_owned(),
            account_id: 0,
            judgments: vec![WireJudgment {
                message_id: "<security@example.com>".to_owned(),
                gain: 0,
            }],
        }],
        mode: ProtoMode::Unspecified as i32,
        limit: 0,
    };

    let report = server
        .client()
        .await
        .evaluate(request)
        .await
        .expect("Evaluate")
        .into_inner();
    server.stop().await;

    let q = &report.per_query[0];
    assert!(q.unresolved.is_empty());
    assert_eq!(q.relevant, 1);
    assert!(
        q.metrics.expect("metrics").ndcg_at_10 > 0.0,
        "an ungraded judgment must still be relevant"
    );
}

#[tokio::test]
async fn lexical_mode_is_honored_and_scores_the_lexical_queries() {
    // `mode` reaching the pipeline is what makes a mode-vs-mode comparison
    // (the "measurably beats pure-BM25" claim in prd.md's success criteria)
    // possible at all, so it has to actually be plumbed rather than ignored.
    let server = TestServer::start().await;
    let report = server
        .client()
        .await
        .evaluate(to_request(&golden_set(), ProtoMode::Lexical))
        .await
        .expect("Evaluate")
        .into_inner();
    server.stop().await;

    let aggregate = report.aggregate.expect("aggregate");
    assert!(
        aggregate.ndcg_at_10 > 0.0,
        "lexical-only retrieval should still answer keyword queries"
    );
}

#[tokio::test]
async fn a_query_with_no_relevant_judgment_is_rejected_as_invalid_argument() {
    // The daemon re-validates rather than trusting the client: an NDCG of
    // 0/0 has no useful value to return, so the request is refused with the
    // specific reason instead of answering with a meaningless zero.
    let server = TestServer::start().await;
    let request = EvaluateRequest {
        corpus: "ad-hoc".to_owned(),
        queries: vec![WireGoldenQuery {
            name: "empty".to_owned(),
            query: "anything".to_owned(),
            account_id: 0,
            judgments: Vec::new(),
        }],
        mode: ProtoMode::Unspecified as i32,
        limit: 0,
    };

    let status = server
        .client()
        .await
        .evaluate(request)
        .await
        .expect_err("must be refused");
    server.stop().await;

    assert_eq!(status.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_duplicate_query_name_is_rejected() {
    let server = TestServer::start().await;
    let judgment = WireJudgment {
        message_id: "<aws-jul@example.com>".to_owned(),
        gain: 3,
    };
    let request = EvaluateRequest {
        corpus: "ad-hoc".to_owned(),
        queries: vec![
            WireGoldenQuery {
                name: "same".to_owned(),
                query: "aws".to_owned(),
                account_id: 0,
                judgments: vec![judgment.clone()],
            },
            WireGoldenQuery {
                name: "same".to_owned(),
                query: "invoice".to_owned(),
                account_id: 0,
                judgments: vec![judgment],
            },
        ],
        mode: ProtoMode::Unspecified as i32,
        limit: 0,
    };

    let status = server
        .client()
        .await
        .evaluate(request)
        .await
        .expect_err("must be refused");
    server.stop().await;

    assert_eq!(status.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn evaluating_an_empty_set_is_rejected_rather_than_reporting_a_perfect_zero() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .evaluate(EvaluateRequest {
            corpus: "ad-hoc".to_owned(),
            queries: Vec::new(),
            mode: ProtoMode::Unspecified as i32,
            limit: 0,
        })
        .await
        .expect_err("must be refused");
    server.stop().await;

    assert_eq!(status.code(), Code::InvalidArgument);
}
