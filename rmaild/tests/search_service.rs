//! Integration test: drive `SearchService` end-to-end against an in-process
//! tonic server over a Unix domain socket — the full pipeline
//! (`QueryPlanner` -> `Fanout` -> `Fuser` -> `FeatureExtractor` -> `L1Ranker`
//! -> `Presenter`) wired through the gRPC surface task 33 owns.
//!
//! `rmaild::search_service`'s own inline unit tests already prove the
//! generation-token replace-and-cancel mechanism in isolation (no database,
//! no gRPC harness). What this suite proves instead is everything that
//! mechanism — and the rest of the wiring — only means something once it is
//! actually reachable over the wire: streamed hits carrying the documented
//! shape, the first hit genuinely arriving before the rest of the page is
//! computed, a fresh request actually cutting an older scan short (not
//! merely discarding its output), and `Explain`'s contributions reconciling
//! with the score the ranker actually produced.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use rmail_core::index::fts::FtsIndex;
use rmail_core::index::semantic::{SemanticIndex, VECTOR_DIM};
use rmail_core::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use rmail_core::repo;
use rmail_core::{Config, Database};
use rmail_proto::v1::search_service_client::SearchServiceClient;
use rmail_proto::v1::{ExplainRequest, Intent as ProtoIntent, SearchHit, SearchRequest};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// How long a stream assertion waits before failing. Generous because these
/// are liveness checks on spawned tasks, not latency measurements (the one
/// genuine latency measurement in this file,
/// `streaming_first_hit_arrives_before_the_full_page_is_computed`, compares
/// two durations against each other rather than against a fixed bound).
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: Cell<i64>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        Self::with_config(Config::default()).await
    }

    async fn with_config(mut config: Config) -> Self {
        // Semantic indexing off by default: the deterministic hash fallback
        // (see `rmaild::serve_uds`'s own identical convention) keeps every
        // test in this file from paying to load — or, on a cold cache,
        // download — an ONNX model it does not need to exercise the search
        // *surface*. `semantic_search_returns_only_dense_sourced_hits` is
        // the one test that needs real dense candidates; it builds its own
        // `SemanticIndex` over the identical deterministic fallback
        // (`embed::hash::HashEmbedder`, matching model id and dimension) and
        // writes directly into `vec_chunks`, which the server's own
        // internally-built fallback embedder can then find.
        config.index.semantic.enabled = false;

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-search-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-search-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let db = Database::open(&db_path).unwrap();

        let n_copy = n;
        let (account_id, mailbox_id) = db
            .with_write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: format!("Personal-{n_copy}"),
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

        Self {
            socket,
            db_path,
            db,
            fts,
            queue,
            account_id,
            mailbox_id,
            next_uid: Cell::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> SearchServiceClient<Channel> {
        SearchServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    /// Insert, extract, and lexically index a message — the real pipeline
    /// (mirrors `rmail_core::retrieve::lexical::tests::Fixture::index`, the
    /// established pattern for seeding FTS-searchable content in this
    /// workspace's tests).
    async fn index(&self, new: repo::NewMessage) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let account_id = if new.account_id != 0 {
            new.account_id
        } else {
            self.account_id
        };
        let mailbox_id = if new.mailbox_id != 0 {
            new.mailbox_id
        } else {
            self.mailbox_id
        };
        let new = repo::NewMessage {
            account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            ..new
        };
        let message_id = self
            .db
            .with_write(move |c| repo::insert_message(c, &new))
            .unwrap();
        extract_message(&self.db, &self.queue, message_id, PRIORITY_NORMAL)
            .await
            .unwrap();
        self.fts.index_message(message_id).await.unwrap();
        message_id
    }

    /// Seed `count` messages that all lexically match `term` (in the body,
    /// padded to `body_repeats` copies of a filler line — bigger for tests
    /// that need Stage 6 presentation work, per candidate, to be heavy
    /// enough to dominate channel/scheduling noise, not just a large
    /// candidate count), for the timing/cancellation tests that need a
    /// corpus large enough for multi-candidate pipeline work to take
    /// measurable time.
    async fn seed_bulk(&self, count: usize, term: &str, body_repeats: usize) -> Vec<i64> {
        let mut ids = Vec::with_capacity(count);
        for i in 0..count {
            let body =
                format!("Quarterly {term} review line item number {i}, filed for the record. ")
                    .repeat(body_repeats);
            let id = self
                .index(repo::NewMessage {
                    subject: Some(format!("{term} note {i}")),
                    body_text: Some(body),
                    date: Some(1_700_000_000 + i as i64),
                    ..Default::default()
                })
                .await;
            ids.push(id);
        }
        ids
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

/// Take the next stream item, failing rather than hanging.
async fn next<S, T>(stream: &mut S) -> T
where
    S: tokio_stream::Stream<Item = Result<T, tonic::Status>> + Unpin,
{
    tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for a stream item")
        .expect("stream ended early")
        .expect("stream item was an error")
}

/// Drain a stream to completion. Panics on a per-item error — callers that
/// expect one should read a single item with [`next_result`] instead.
async fn drain<S, T>(stream: &mut S) -> Vec<T>
where
    S: tokio_stream::Stream<Item = Result<T, tonic::Status>> + Unpin,
{
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(item))) => out.push(item),
            Ok(Some(Err(status))) => panic!("stream item was an error: {status}"),
            Ok(None) => break,
            Err(_) => panic!("timed out draining stream"),
        }
    }
    out
}

/// Take the next stream item as a `Result`, for a test that expects the
/// item itself to carry an error status (`Search`'s streaming error path).
async fn next_result<S, T>(stream: &mut S) -> Result<T, tonic::Status>
where
    S: tokio_stream::Stream<Item = Result<T, tonic::Status>> + Unpin,
{
    tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("timed out waiting for a stream item")
        .expect("stream ended before delivering the expected error")
}

fn search_request(query: &str) -> SearchRequest {
    SearchRequest {
        query: query.to_owned(),
        ..Default::default()
    }
}

fn hit_subject(hit: &SearchHit) -> Option<String> {
    hit.message.as_ref().and_then(|m| m.subject.clone())
}

// ---------------------------------------------------------------------------
// Shape: SearchHit carries score, highlighted snippet, sources
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_streams_ranked_hits_with_score_snippet_and_sources() {
    let server = TestServer::start().await;
    let strong = server
        .index(repo::NewMessage {
            subject: Some("Quarterly budgetary review".to_owned()),
            body_text: Some("The budgetary review is attached for the quarter.".to_owned()),
            from_addr: Some("finance@example.com".to_owned()),
            date: Some(1_700_000_000),
            ..Default::default()
        })
        .await;
    server
        .index(repo::NewMessage {
            subject: Some("Team lunch".to_owned()),
            body_text: Some("Let's get lunch on Friday.".to_owned()),
            date: Some(1_700_000_100),
            ..Default::default()
        })
        .await;

    let mut client = server.client().await;
    let mut stream = client
        .search(search_request("budgetary"))
        .await
        .unwrap()
        .into_inner();

    let hit = next(&mut stream).await;
    assert_eq!(
        hit_subject(&hit).as_deref(),
        Some("Quarterly budgetary review")
    );
    let message = hit.message.expect("message payload present");
    assert_eq!(message.id, strong);
    assert_eq!(message.account_id, server.account_id);
    assert_eq!(message.mailbox_id, server.mailbox_id);
    assert!(
        hit.score > 0.0,
        "a lexically-matched hit must score above 0"
    );

    let snippet = hit.snippet.expect("snippet present");
    assert!(!snippet.text.is_empty());
    assert!(
        !snippet.highlights.is_empty(),
        "a lexical hit's snippet must carry highlight ranges, not embedded markup"
    );
    for range in &snippet.highlights {
        assert!(range.start < range.end);
        assert!((range.end as usize) <= snippet.text.len());
    }

    assert!(
        hit.sources.iter().any(|s| s == "lexical"),
        "expected \"lexical\" among sources, got {:?}",
        hit.sources
    );
    assert!(hit.why.is_none(), "explain was not requested");
    assert!(hit.thread_id.is_none());
    assert!(hit.thread_collapsed.is_empty());
    assert!(hit.near_duplicates.is_empty());

    // Only the matching message comes back for this query.
    let rest = drain(&mut stream).await;
    assert!(rest.is_empty(), "unexpected extra hits: {rest:?}");

    server.stop().await;
}

#[tokio::test]
async fn search_with_unknown_account_id_streams_a_not_found_error() {
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            subject: Some("budgetary".to_owned()),
            body_text: Some("budgetary".to_owned()),
            ..Default::default()
        })
        .await;

    let mut client = server.client().await;
    let mut req = search_request("budgetary");
    req.account_id = 999_999;
    let mut stream = client.search(req).await.unwrap().into_inner();

    let status = next_result(&mut stream)
        .await
        .expect_err("an unknown account must surface as a stream error");
    assert_eq!(status.code(), Code::NotFound);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Explain: reconciling contributions, on both the inline flag and the RPC
// ---------------------------------------------------------------------------

#[tokio::test]
async fn search_explain_flag_reconciles_with_the_streamed_score() {
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            subject: Some("budgetary planning".to_owned()),
            body_text: Some("Please review the budgetary plan for next quarter.".to_owned()),
            ..Default::default()
        })
        .await;

    let mut client = server.client().await;
    let mut req = search_request("budgetary");
    req.explain = true;
    let mut stream = client.search(req).await.unwrap().into_inner();

    let hit = next(&mut stream).await;
    let why = hit.why.expect("why must be present when explain=true");

    assert_eq!(
        why.score, hit.score,
        "RankExplanation.score must be the same score the streamed hit carries"
    );
    assert!(
        !why.features.is_empty(),
        "at least one feature contribution must be reported"
    );
    let summed: f64 = why.features.iter().map(|f| f.weighted_contribution).sum();
    assert!(
        (summed - why.score).abs() < 1e-6,
        "contributions ({summed}) must sum to the reported score ({})",
        why.score
    );
    for feature in &why.features {
        assert!(
            (feature.weighted_contribution - feature.weight * feature.value).abs() < 1e-9,
            "weighted_contribution must equal weight * value for {}",
            feature.name
        );
    }
    // Feature names come from `FeatureName::as_str()`'s stable strings, not
    // ad hoc labels — spot-check one that a subject-line match must produce.
    assert!(
        why.features.iter().any(|f| f.name == "bm25_subject"),
        "expected a bm25_subject contribution, got {:?}",
        why.features.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
    assert!(
        why.sources.iter().any(|s| s == "lexical"),
        "why.sources should agree with the hit's own sources"
    );

    server.stop().await;
}

#[tokio::test]
async fn explain_rpc_reconciles_a_message_outside_the_ranked_page() {
    // A tiny `top_k_rerank` so the weakest of four matching candidates never
    // survives Stage 4 — proving `Explain` reads Stage 3's full feature list
    // rather than a ranked/presented page.
    let mut config = Config::default();
    config.index.semantic.enabled = false;
    config.search.top_k_rerank = 2;
    let server = TestServer::with_config(config).await;

    server
        .index(repo::NewMessage {
            subject: Some("budgetary budgetary budgetary".to_owned()),
            body_text: Some("budgetary".to_owned()),
            ..Default::default()
        })
        .await;
    server
        .index(repo::NewMessage {
            subject: Some("budgetary review".to_owned()),
            body_text: Some("quarterly budgetary notes".to_owned()),
            ..Default::default()
        })
        .await;
    server
        .index(repo::NewMessage {
            subject: Some("misc".to_owned()),
            body_text: Some("a passing mention of the budgetary topic".to_owned()),
            ..Default::default()
        })
        .await;
    let weakest = server
        .index(repo::NewMessage {
            subject: Some("unrelated".to_owned()),
            body_text: Some(
                "regarding budgetary matters in general terms only, once, in passing".to_owned(),
            ),
            ..Default::default()
        })
        .await;

    let mut client = server.client().await;

    // Confirm the premise: the weakest match does not appear in a real
    // search response at this `top_k_rerank`.
    let mut stream = client
        .search(search_request("budgetary"))
        .await
        .unwrap()
        .into_inner();
    let paged = drain(&mut stream).await;
    assert!(
        paged.len() <= 2,
        "top_k_rerank=2 must cap the streamed page, got {} hits",
        paged.len()
    );
    assert!(
        !paged
            .iter()
            .any(|h| h.message.as_ref().map(|m| m.id) == Some(weakest)),
        "the weakest candidate must not have made this request's own page"
    );

    // `Explain` still answers for it.
    let explanation = client
        .explain(ExplainRequest {
            query: "budgetary".to_owned(),
            message_id: weakest,
            ..Default::default()
        })
        .await
        .expect("Explain must find a message Stage 3 saw, even off the page")
        .into_inner();

    assert!(!explanation.features.is_empty());
    let summed: f64 = explanation
        .features
        .iter()
        .map(|f| f.weighted_contribution)
        .sum();
    assert!(
        (summed - explanation.score).abs() < 1e-6,
        "contributions ({summed}) must sum to the reported score ({})",
        explanation.score
    );
    assert!(explanation.sources.iter().any(|s| s == "lexical"));
    let matched = explanation.matched.expect("a matched span for a body hit");
    assert!(!matched.text.is_empty());

    server.stop().await;
}

#[tokio::test]
async fn explain_rpc_not_found_for_a_message_the_query_never_matched() {
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            subject: Some("budgetary".to_owned()),
            body_text: Some("budgetary".to_owned()),
            ..Default::default()
        })
        .await;
    let unrelated = server
        .index(repo::NewMessage {
            subject: Some("aquarium maintenance".to_owned()),
            body_text: Some("remember to clean the aquarium filter".to_owned()),
            ..Default::default()
        })
        .await;

    let mut client = server.client().await;
    let status = client
        .explain(ExplainRequest {
            query: "budgetary".to_owned(),
            message_id: unrelated,
            ..Default::default()
        })
        .await
        .expect_err("a message with no lexical/entity/structured overlap should not be found");
    assert_eq!(status.code(), Code::NotFound);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Streaming is incremental
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_first_hit_arrives_before_the_full_page_is_computed() {
    // Comparing "time to the first hit" against "time for everything after
    // it" (an earlier version of this test did exactly that) is contaminated
    // by a large *shared* one-time cost neither phase is responsible for:
    // `QueryPlanner::plan_at` alone makes several sequential DB round trips
    // (spell-fix, entity resolution, PMI synonym expansion, query embedding)
    // before Phase 1 even starts, so "time to first" is dominated by query
    // *understanding*, not by whether presentation is incremental — on a
    // small/fast corpus that setup cost can legitimately exceed Phase 2's
    // own work and make an otherwise-correct implementation look wrong.
    //
    // The signature this test actually needs to detect is narrower and does
    // not require subtracting that shared cost out: with the two-phase
    // design (see `rmaild::search_service`'s own module docs), hit 2 is the
    // *first* item Phase 2's single batched `Presenter::present` call
    // produces, so the whole batch's cost lands in the gap between hit 1 and
    // hit 2 — while hits 2..N are all already computed by the time hit 2 is
    // sent, so the gaps *between* them are just channel/scheduling overhead.
    // A "compute everything, then send it all" implementation would show the
    // opposite: a uniformly small gap everywhere, hit 1 included.
    //
    // Phase 2's batch has to be *heavy enough per item* to clear the noise
    // floor of channel/scheduling overhead — a small corpus of short bodies
    // makes `Presenter::present`'s own batch (metadata fetch, snippet
    // extraction, MMR fingerprinting) fast enough in a warm, cached SQLite
    // file that it is indistinguishable from microsecond-scale channel
    // sends. Bodies near `present::snippet::MAX_SOURCE_CHARS`, a page sized
    // to `top_k_rerank`, and `Intent::Exploratory` (which turns on MMR —
    // SimHash-fingerprinting every selected candidate, real CPU work Phase 2
    // would otherwise skip) all push Phase 2's actual cost well above that
    // floor without changing what property is being proven.
    let server = TestServer::start().await;
    server.seed_bulk(100, "budgetary", 40).await;

    let mut client = server.client().await;

    let heavy_page = || SearchRequest {
        query: "budgetary".to_owned(),
        limit: 50,
        intent: ProtoIntent::Exploratory as i32,
        ..Default::default()
    };

    // Warm-up: prime connection pools/caches so the timed run below is not
    // skewed by one-time setup cost unrelated to per-request pipeline work.
    let mut warm = client.search(heavy_page()).await.unwrap().into_inner();
    let _ = drain(&mut warm).await;

    let mut stream = client.search(heavy_page()).await.unwrap().into_inner();

    let _first = next(&mut stream).await;
    let t_first = Instant::now();
    let _second = next(&mut stream).await;
    let t_second = Instant::now();
    let gap_first_to_second = t_second - t_first;

    let mut rest_arrivals = Vec::new();
    loop {
        match tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(_))) => rest_arrivals.push(Instant::now()),
            Ok(Some(Err(status))) => panic!("stream item was an error: {status}"),
            Ok(None) => break,
            Err(_) => panic!("timed out draining stream"),
        }
    }
    assert!(
        rest_arrivals.len() >= 5,
        "the corpus is large enough that several more hits should page through \
         after the second, got {}",
        rest_arrivals.len()
    );

    // The average per-item gap *within* Phase 2's already-computed batch —
    // hits 3, 4, 5, ... were all produced by the same `present()` call that
    // produced hit 2, so delivering them costs essentially nothing more.
    let mut previous = t_second;
    let mut total_rest_gap = Duration::ZERO;
    for &arrival in &rest_arrivals {
        total_rest_gap += arrival.duration_since(previous);
        previous = arrival;
    }
    let avg_rest_gap = total_rest_gap / u32::try_from(rest_arrivals.len()).unwrap_or(1).max(1);

    assert!(
        gap_first_to_second > avg_rest_gap.saturating_mul(3),
        "the gap between hit 1 and hit 2 ({gap_first_to_second:?}) should reflect Phase \
         2's whole batch computation, and be well above the average per-item gap once \
         that batch is already computed ({avg_rest_gap:?} across {} later hits) — \
         otherwise hits are not being streamed incrementally",
        rest_arrivals.len()
    );

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Cancellation actually halts the superseded scan
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_fresh_search_request_cancels_the_prior_stream() {
    let server = TestServer::start().await;
    server.seed_bulk(150, "budgetary", 6).await;
    server
        .index(repo::NewMessage {
            subject: Some("aquarium maintenance".to_owned()),
            body_text: Some("remember to clean the aquarium filter this weekend".to_owned()),
            ..Default::default()
        })
        .await;

    let mut client = server.client().await;

    // Request A: broad, matches the whole 150-message corpus — issued and
    // its response *headers* obtained, but not read from yet, so its
    // background pipeline task is still free-running.
    let mut stream_a = client
        .search(search_request("budgetary"))
        .await
        .unwrap()
        .into_inner();

    // Request B supersedes it immediately — no read on `stream_a` happened
    // in between, so this is the earliest point a second call could land.
    let mut stream_b = client
        .search(search_request("aquarium"))
        .await
        .unwrap()
        .into_inner();
    let b_hits = drain(&mut stream_b).await;
    assert_eq!(
        b_hits.len(),
        1,
        "the superseding request must complete normally"
    );
    assert_eq!(
        hit_subject(&b_hits[0]).as_deref(),
        Some("aquarium maintenance")
    );

    // Whatever `stream_a` produced before it was cancelled.
    let a_hits = drain(&mut stream_a).await;

    // A control run of A's identical, unsuperseded query proves what the
    // full page *would* contain — the comparison is what proves the
    // cancelled run was actually cut short, not merely a coincidentally
    // short "real" result.
    let mut control_stream = client
        .search(search_request("budgetary"))
        .await
        .unwrap()
        .into_inner();
    let control_hits = drain(&mut control_stream).await;
    assert!(
        control_hits.len() > 1,
        "the control run should page through more than a single hit"
    );

    assert!(
        a_hits.len() < control_hits.len(),
        "a superseded stream produced {} hits, the same as an unsuperseded control ({}) — \
         the generation token did not actually halt the older scan",
        a_hits.len(),
        control_hits.len()
    );

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Semantic: dense-only, regardless of what else would match
// ---------------------------------------------------------------------------

#[tokio::test]
async fn semantic_search_returns_only_dense_sourced_hits() {
    let server = TestServer::start().await;
    let message_id = server
        .index(repo::NewMessage {
            subject: Some("octopus submarine expedition notes".to_owned()),
            body_text: Some(
                "The octopus submarine expedition surveyed the trench for three days.".to_owned(),
            ),
            ..Default::default()
        })
        .await;

    // Embed the message directly into `vec_chunks`, over the identical
    // deterministic fallback embedder (`embed::hash::HashEmbedder`, same
    // model id and `VECTOR_DIM`) the server's own `SearchApi` builds when
    // `index.semantic.enabled = false` — see `TestServer::with_config`'s own
    // doc comment.
    let embedder: std::sync::Arc<dyn rmail_core::embed::Embedder> =
        std::sync::Arc::new(rmail_core::embed::hash::HashEmbedder::new(VECTOR_DIM));
    let semantic_index = SemanticIndex::new(
        server.db.clone(),
        embedder,
        &rmail_core::config::IndexSemanticConfig::default(),
    );
    semantic_index.index_message(message_id).await.unwrap();

    let mut client = server.client().await;
    let mut stream = client
        .semantic(search_request("octopus submarine expedition"))
        .await
        .unwrap()
        .into_inner();

    let hit = next(&mut stream).await;
    assert_eq!(
        hit.message.as_ref().map(|m| m.id),
        Some(message_id),
        "the only embedded message should be the one Semantic finds"
    );
    assert_eq!(
        hit.sources,
        vec!["dense".to_owned()],
        "Semantic must never surface a non-dense source, regardless of what \
         a hybrid Search would also find lexically for the same text"
    );
    assert!(hit.score > 0.0);

    let rest = drain(&mut stream).await;
    assert!(rest.is_empty());

    server.stop().await;
}
