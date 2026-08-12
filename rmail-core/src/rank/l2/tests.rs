//! What task 51's acceptance bullets ask to be *proven*, not asserted:
//!
//! - **Cross-encoder reorder** — [`cross_encoder_reorders_the_l1_window`]
//!   and [`a_reranked_order_survives_a_score_sort`]: a backend on the
//!   cross-encoder seam actually changes the order the pipeline hands Stage
//!   6, *and* that order survives the score sort `present::Presenter`
//!   performs on it (the failure mode a naive "reorder the vec" would have).
//! - **Claude listwise via mock** — [`claude_listwise_reorders_and_explains`]
//!   drives the real [`ClaudeReranker`] against a mock [`Provider`],
//!   including redaction, the structured-output parse, and the per-result
//!   "why".
//! - **Degrade on error** — [`a_failing_backend_keeps_the_l1_order`],
//!   [`a_slow_backend_degrades_at_its_deadline`],
//!   [`a_missing_document_keeps_the_l1_order`],
//!   [`an_unconfigured_backend_keeps_the_l1_order`],
//!   [`a_backend_that_answers_about_other_candidates_is_discarded`],
//!   [`a_provider_failure_keeps_the_l1_order`], and
//!   [`an_exhausted_budget_keeps_the_l1_order`]: every failure class ends in
//!   the same place, and none of them is an error the caller can see.
//! - **Cache key** — [`cache_key_ignores_candidate_order`],
//!   [`cache_key_separates_query_model_and_candidate_set`],
//!   [`the_listwise_cache_serves_a_repeat_query_without_a_provider_call`],
//!   and [`the_cache_evicts_least_recently_used_entries`].
//!
//! Nothing here needs a network or an ONNX model file: the ONNX backend's
//! own reachable behaviour is its *unavailability* message
//! ([`an_unprovisioned_cross_encoder_says_how_to_fix_it`]), and the reorder
//! logic is exercised through [`StubReranker`], a deterministic backend on
//! the same trait.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use super::*;
use crate::ai::provider::{ChatRequest, ChatResponse, Provider, ProviderStream, StopReason, Usage};
use crate::ai::PolicyEngine;
use crate::config::{AiLimits, AiPrivacy, Config, RerankerConfig, SearchConfig};
use crate::repo;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-rank-l2-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open test db");
        let (account_id, mailbox_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
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
            .await
            .expect("seed account/mailbox");
        Self {
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    /// A message plus its `index_content` body row, written directly so this
    /// file has byte-exact control over what a reranker reads — the same
    /// choice `present::tests` makes, for the same reason.
    async fn insert_message(&self, subject: &str, body: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
            uid,
            uidvalidity: 1,
            subject: Some(subject.to_owned()),
            from_addr: Some("sender@example.com".to_owned()),
            from_name: Some("A Sender".to_owned()),
            date: Some(1_700_000_000),
            ..Default::default()
        };
        let message_id = self
            .db
            .write(move |c| repo::insert_message(c, &new))
            .await
            .expect("insert message");
        let body = body.to_owned();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO index_content \
                     (message_id, part, text, chars, content_hash, extractor) \
                     VALUES (?1, 'body', ?2, ?3, X'00', 'test')",
                    rusqlite::params![message_id, body, body.chars().count() as i64],
                )
            })
            .await
            .expect("insert body");
        message_id
    }

    /// Three messages, and the L1 ranking that puts them in insertion order
    /// with strictly decreasing scores.
    async fn three(&self) -> (Vec<i64>, Vec<RankedCandidate>) {
        let mut ids = Vec::new();
        for (subject, body) in [
            ("Invoice reminder", "a polite nudge about an unpaid invoice"),
            ("Invoice #338 Acme", "the actual invoice, total $4,200"),
            ("Lunch", "unrelated chatter about lunch on friday"),
        ] {
            ids.push(self.insert_message(subject, body).await);
        }
        let ranked = vec![
            RankedCandidate {
                message_id: ids[0],
                score: 9.0,
            },
            RankedCandidate {
                message_id: ids[1],
                score: 6.0,
            },
            RankedCandidate {
                message_id: ids[2],
                score: 3.0,
            },
        ];
        (ids, ranked)
    }

    fn search_config(&self) -> SearchConfig {
        SearchConfig {
            rerank: Rerank::CrossEncoder,
            reranker: RerankerConfig {
                // Short enough that a deliberately-slow stub trips it inside
                // a test's own patience, long enough that ordinary CI
                // scheduling jitter never does.
                timeout: crate::config::HumanDuration::new(Duration::from_millis(250)),
                ..RerankerConfig::default()
            },
            ..SearchConfig::default()
        }
    }

    fn stage(&self, search: &SearchConfig) -> L2Stage {
        self.stage_with_policy(search, &Config::default())
    }

    /// A stage whose AI policy comes from `config` — the seam the
    /// policy-gate tests drive.
    fn stage_with_policy(&self, search: &SearchConfig, config: &Config) -> L2Stage {
        let policy = Arc::new(PolicyEngine::from_config(config).expect("valid ai policy"));
        L2Stage::new(self.db.clone(), search, policy, None)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn ids_of(ranked: &[RankedCandidate]) -> Vec<i64> {
    ranked.iter().map(|c| c.message_id).collect()
}

// ---------------------------------------------------------------------------
// A deterministic backend on the real trait
// ---------------------------------------------------------------------------

/// What a stub backend does when asked.
#[derive(Debug, Clone)]
enum StubBehaviour {
    /// Score each candidate by its position in this id list (earlier = better).
    Order(Vec<i64>),
    /// Fail.
    Fail,
    /// Take this long before answering — for the deadline test.
    Slow(Duration),
    /// Answer about candidates the stage never asked about.
    Foreign,
    /// Give every candidate the same score, so only the tie-break decides.
    Flat,
    /// Report itself unavailable before any document is read.
    NotReady,
}

#[derive(Debug)]
struct StubReranker {
    behaviour: StubBehaviour,
    calls: Arc<AtomicUsize>,
    /// Which half of the AI policy gate this stub stands in for.
    needs_network: bool,
}

impl StubReranker {
    /// Returns the backend behind the trait object the stage takes, plus a
    /// call counter the test keeps — which is why this is not a `new`
    /// returning `Self`.
    fn build(behaviour: StubBehaviour) -> (Arc<dyn Reranker>, Arc<AtomicUsize>) {
        Self::build_with(behaviour, false)
    }

    /// As [`Self::build`], but standing in for a network backend — the arm
    /// of the policy gate that requires `permits_network`.
    fn build_networked(behaviour: StubBehaviour) -> (Arc<dyn Reranker>, Arc<AtomicUsize>) {
        Self::build_with(behaviour, true)
    }

    fn build_with(
        behaviour: StubBehaviour,
        needs_network: bool,
    ) -> (Arc<dyn Reranker>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let stub = Self {
            behaviour,
            calls: Arc::clone(&calls),
            needs_network,
        };
        (Arc::new(stub) as Arc<dyn Reranker>, calls)
    }
}

#[async_trait]
impl Reranker for StubReranker {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn needs_network(&self) -> bool {
        self.needs_network
    }

    async fn ready(&self, _cancel: &CancellationToken) -> Result<(), Error> {
        match self.behaviour {
            StubBehaviour::NotReady => Err(Error::failed_precondition(
                "the stub model is not provisioned".to_owned(),
            )),
            _ => Ok(()),
        }
    }

    async fn rerank(
        &self,
        _query: &str,
        candidates: &[RerankCandidate],
        cancel: &CancellationToken,
    ) -> Result<Vec<RerankVerdict>, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.behaviour {
            StubBehaviour::NotReady | StubBehaviour::Fail => {
                Err(Error::internal("stub backend failed".to_owned()))
            }
            StubBehaviour::Slow(delay) => {
                tokio::select! {
                    () = tokio::time::sleep(*delay) => {}
                    () = cancel.cancelled() => {}
                }
                Ok(candidates
                    .iter()
                    .map(|c| RerankVerdict {
                        message_id: c.message_id,
                        score: 1.0,
                        why: None,
                    })
                    .collect())
            }
            StubBehaviour::Foreign => Ok(candidates
                .iter()
                .map(|c| RerankVerdict {
                    // An id no window ever contains.
                    message_id: c.message_id + 10_000,
                    score: 1.0,
                    why: None,
                })
                .collect()),
            StubBehaviour::Flat => Ok(candidates
                .iter()
                .map(|c| RerankVerdict {
                    message_id: c.message_id,
                    score: 1.0,
                    why: None,
                })
                .collect()),
            StubBehaviour::Order(order) => Ok(candidates
                .iter()
                .map(|c| {
                    let position = order
                        .iter()
                        .position(|id| *id == c.message_id)
                        .unwrap_or(order.len());
                    RerankVerdict {
                        message_id: c.message_id,
                        score: (order.len() - position) as f64,
                        why: Some(format!("stub reason for {}", c.message_id)),
                    }
                })
                .collect()),
        }
    }
}

// ---------------------------------------------------------------------------
// A mock provider, for the real ClaudeReranker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum MockReply {
    Ok(String),
    Fail,
}

#[derive(Debug)]
struct MockProvider {
    replies: Mutex<VecDeque<MockReply>>,
    calls: Arc<AtomicUsize>,
    /// Every user-turn prompt this provider was handed, so a test can assert
    /// what actually crossed the boundary.
    prompts: Mutex<Vec<String>>,
}

impl MockProvider {
    fn new(replies: Vec<MockReply>) -> (Arc<Self>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(Self {
            replies: Mutex::new(replies.into()),
            calls: Arc::clone(&calls),
            prompts: Mutex::new(Vec::new()),
        });
        (provider, calls)
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(
                request
                    .messages
                    .iter()
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        let reply = self
            .replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match reply {
            Some(MockReply::Ok(text)) => Ok(ChatResponse {
                id: "msg_mock".to_owned(),
                model: "mock-model".to_owned(),
                stop_reason: StopReason::EndTurn,
                text,
                usage: Usage::default(),
            }),
            Some(MockReply::Fail) | None => {
                Err(Error::unavailable("mock provider unavailable".to_owned()))
            }
        }
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        Err(Error::internal("the rerank path never streams".to_owned()))
    }
}

/// A listwise answer naming `labels` in order, with a reason per label.
fn listwise(labels: &[usize]) -> String {
    let results: Vec<serde_json::Value> = labels
        .iter()
        .map(|label| serde_json::json!({"label": label, "why": format!("reason {label}")}))
        .collect();
    serde_json::json!({ "results": results }).to_string()
}

fn claude_stage(fixture: &Fixture, provider: Arc<dyn Provider>, search: &SearchConfig) -> L2Stage {
    let claude: Arc<dyn Reranker> = Arc::new(ClaudeReranker::new(
        provider,
        fixture.db.clone(),
        &search.reranker,
        AiLimits::default(),
        AiPrivacy::default(),
        Arc::new(tokio::sync::Semaphore::new(4)),
        Arc::new(crate::ai::queue::RateLimiter::new(600)),
    ));
    let policy = Arc::new(PolicyEngine::from_config(&Config::default()).expect("valid ai policy"));
    L2Stage::new(fixture.db.clone(), search, policy, Some(claude))
}

// ---------------------------------------------------------------------------
// Reorder
// ---------------------------------------------------------------------------

/// The headline behaviour: a cross-encoder-seam backend that prefers the
/// second candidate actually moves it to the top of what Stage 6 receives.
#[tokio::test]
async fn cross_encoder_reorders_the_l1_window() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    // Best-first by the backend's judgment: the real invoice, then lunch,
    // then the reminder — deliberately unlike the L1 order.
    let (stub, calls) = StubReranker::build(StubBehaviour::Order(vec![ids[1], ids[2], ids[0]]));
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::CrossEncoder);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(out.backend, Some("stub"));
    assert_eq!(ids_of(&out.ranked), vec![ids[1], ids[2], ids[0]]);
    // The window's own scores, re-assigned to the new order — never the
    // backend's own numbers, which are on a different scale entirely.
    assert_eq!(
        out.ranked.iter().map(|c| c.score).collect::<Vec<_>>(),
        vec![9.0, 6.0, 3.0]
    );
}

/// The trap a "reorder the `Vec` and keep the scores" implementation falls
/// into: `present::Presenter` re-sorts by score, so an order that is not
/// expressed *in the scores* is silently undone one stage later. This test
/// applies exactly that sort and asserts the reranked order survives it,
/// including the equal-L1-score case where the sort's `message_id`
/// tie-break would otherwise decide.
#[tokio::test]
async fn a_reranked_order_survives_a_score_sort() {
    let fixture = Fixture::open().await;
    let (ids, _) = fixture.three().await;
    // Two candidates with byte-identical L1 scores — near-duplicate mail
    // with identical feature vectors, which is the only way this happens.
    let ranked = vec![
        RankedCandidate {
            message_id: ids[0],
            score: 5.0,
        },
        RankedCandidate {
            message_id: ids[1],
            score: 5.0,
        },
        RankedCandidate {
            message_id: ids[2],
            score: 1.0,
        },
    ];
    let search = fixture.search_config();
    let (stub, _) = StubReranker::build(StubBehaviour::Order(vec![ids[2], ids[1], ids[0]]));
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::CrossEncoder);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(ids_of(&out.ranked), vec![ids[2], ids[1], ids[0]]);

    // `present::strict_score_order`'s exact comparator.
    let mut sorted = out.ranked.clone();
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.message_id.cmp(&b.message_id))
    });
    assert_eq!(
        ids_of(&sorted),
        vec![ids[2], ids[1], ids[0]],
        "a score sort must not undo the rerank"
    );
}

/// A backend that scores everything identically must leave the L1 order
/// exactly as it found it — the tie-break is the incoming rank, not the
/// message id and not the hash order of a map.
#[tokio::test]
async fn a_flat_scoring_backend_preserves_the_l1_order() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (stub, _) = StubReranker::build(StubBehaviour::Flat);
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::CrossEncoder);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(ids_of(&out.ranked), ids);
}

/// Candidates past `claude_max_candidates` are not sent to the listwise
/// backend and keep their L1 order *below* every reranked one.
#[tokio::test]
async fn the_listwise_window_is_capped_and_the_tail_is_untouched() {
    let fixture = Fixture::open().await;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(
            fixture
                .insert_message(&format!("subject {i}"), &format!("body {i}"))
                .await,
        );
    }
    let ranked: Vec<RankedCandidate> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| RankedCandidate {
            message_id: *id,
            score: 10.0 - i as f64,
        })
        .collect();
    let mut search = fixture.search_config();
    search.reranker.claude_max_candidates = 3;
    // The stub would put `ids[4]` first *if it were ever shown it*. It is
    // outside the cap, so it must stay last — which is what makes this test
    // fail if the window is not actually capped, rather than merely asserting
    // an order the uncapped code would also produce.
    let (stub, _) = StubReranker::build(StubBehaviour::Order(vec![
        ids[4], ids[2], ids[1], ids[0], ids[3],
    ]));
    let stage = fixture
        .stage(&search)
        .with_backends(None, Some(stub))
        .with_policy(Rerank::Claude);

    let out = stage
        .rerank(
            "subject",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(
        ids_of(&out.ranked),
        vec![ids[2], ids[1], ids[0], ids[3], ids[4]]
    );
    assert!(
        out.ranked[2].score > out.ranked[3].score,
        "a reranked candidate must never fall below an un-reranked one"
    );
}

// ---------------------------------------------------------------------------
// Policy resolution
// ---------------------------------------------------------------------------

/// `search.rerank = "auto"` is prd.md's rule: the local backend while
/// typing, the hosted one for an explicit deep search.
#[tokio::test]
async fn auto_picks_the_cross_encoder_interactively_and_claude_for_deep_search() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (local, local_calls) =
        StubReranker::build(StubBehaviour::Order(vec![ids[1], ids[0], ids[2]]));
    let (hosted, hosted_calls) =
        StubReranker::build(StubBehaviour::Order(vec![ids[2], ids[1], ids[0]]));
    let stage = fixture
        .stage(&search)
        .with_backends(Some(local), Some(hosted))
        .with_policy(Rerank::Auto);
    let cancel = CancellationToken::new();

    let interactive = stage
        .rerank("invoice", &ranked, SearchKind::Interactive, &cancel)
        .await;
    assert_eq!(ids_of(&interactive.ranked), vec![ids[1], ids[0], ids[2]]);
    assert_eq!(local_calls.load(Ordering::SeqCst), 1);
    assert_eq!(hosted_calls.load(Ordering::SeqCst), 0);

    let deep = stage
        .rerank("invoice", &ranked, SearchKind::Deep, &cancel)
        .await;
    assert_eq!(ids_of(&deep.ranked), vec![ids[2], ids[1], ids[0]]);
    assert_eq!(local_calls.load(Ordering::SeqCst), 1);
    assert_eq!(hosted_calls.load(Ordering::SeqCst), 1);
}

/// `off` calls no backend at all — not even to have its answer discarded.
#[tokio::test]
async fn off_never_calls_a_backend() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (stub, calls) = StubReranker::build(StubBehaviour::Order(vec![ids[2], ids[1], ids[0]]));
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::Off);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

// ---------------------------------------------------------------------------
// Degradation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failing_backend_keeps_the_l1_order() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (stub, calls) = StubReranker::build(StubBehaviour::Fail);
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::CrossEncoder);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the backend really ran");
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
    assert!(out.reasons.is_empty());
}

/// prd.md's Stage 5 is an optional precision improvement, never a latency
/// cliff: a backend that overruns its budget is abandoned and the L1 order
/// stands.
#[tokio::test]
async fn a_slow_backend_degrades_at_its_deadline() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (stub, _) = StubReranker::build(StubBehaviour::Slow(Duration::from_secs(30)));
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::CrossEncoder);

    let started = std::time::Instant::now();
    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the stage must not wait for a backend past its own deadline"
    );
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

/// A candidate whose text could not be read is not a candidate this stage
/// can judge — and reranking the rest would sink it for a reason that has
/// nothing to do with relevance.
#[tokio::test]
async fn a_missing_document_keeps_the_l1_order() {
    let fixture = Fixture::open().await;
    let (ids, mut ranked) = fixture.three().await;
    // A message id no row exists for.
    let ghost = ids.iter().copied().max().unwrap_or(0) + 5_000;
    ranked.push(RankedCandidate {
        message_id: ghost,
        score: 1.0,
    });
    let search = fixture.search_config();
    let (stub, calls) = StubReranker::build(StubBehaviour::Order(vec![ids[2], ids[1], ids[0]]));
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::CrossEncoder);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a partial document set must not reach a backend at all"
    );
    assert_eq!(ids_of(&out.ranked), ids_of(&ranked));
}

/// The default configuration is `rerank = "auto"` with no cross-encoder
/// model provisioned, so this is the *common* path, not an edge case: an
/// unready backend must short-circuit before the stage reads (and renders)
/// up to `top_k_rerank` message bodies for a backend that was always going
/// to refuse.
#[tokio::test]
async fn an_unready_backend_short_circuits_before_any_document_is_read() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (stub, calls) = StubReranker::build(StubBehaviour::NotReady);
    // A stage with no document source at all: reaching the fetch would
    // degrade for a *different* reason, so the only way this test can
    // observe the right behaviour is that the fetch is never attempted.
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::CrossEncoder);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an unready backend must never be asked to rerank"
    );
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

/// A rerank is the only stage of search that reads message *text*, so it is
/// the only one that has to honor `ai.policy`. `local_only` means exactly
/// what it says: the on-device cross-encoder may read the mail, a Claude
/// listwise pass may not.
#[tokio::test]
async fn ai_policy_local_only_permits_the_local_backend_and_not_the_network_one() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let config = Config {
        ai: crate::config::AiConfig {
            policy: crate::config::AiPolicyConfig {
                default_mode: crate::ai::AiPolicyMode::LocalOnly,
                ..crate::config::AiPolicyConfig::default()
            },
            ..crate::config::AiConfig::default()
        },
        ..Config::default()
    };
    let (local, local_calls) =
        StubReranker::build(StubBehaviour::Order(vec![ids[2], ids[1], ids[0]]));
    let (hosted, hosted_calls) =
        StubReranker::build_networked(StubBehaviour::Order(vec![ids[2], ids[1], ids[0]]));
    let stage = fixture
        .stage_with_policy(&search, &config)
        .with_backends(Some(local), Some(hosted));
    let cancel = CancellationToken::new();

    let local_run = stage
        .clone()
        .with_policy(Rerank::CrossEncoder)
        .rerank("invoice", &ranked, SearchKind::Interactive, &cancel)
        .await;
    assert_eq!(local_calls.load(Ordering::SeqCst), 1);
    assert_eq!(ids_of(&local_run.ranked), vec![ids[2], ids[1], ids[0]]);

    let hosted_run = stage
        .with_policy(Rerank::Claude)
        .rerank("invoice", &ranked, SearchKind::Deep, &cancel)
        .await;
    assert_eq!(
        hosted_calls.load(Ordering::SeqCst),
        0,
        "local_only mail must never reach a network reranker"
    );
    assert_eq!(hosted_run.backend, None);
    assert_eq!(ids_of(&hosted_run.ranked), ids);
}

/// `forbidden` is the hard opt-out: not even a local model may read the mail.
#[tokio::test]
async fn ai_policy_forbidden_blocks_even_the_local_backend() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let config = Config {
        ai: crate::config::AiConfig {
            policy: crate::config::AiPolicyConfig {
                default_mode: crate::ai::AiPolicyMode::Forbidden,
                ..crate::config::AiPolicyConfig::default()
            },
            ..crate::config::AiConfig::default()
        },
        ..Config::default()
    };
    let (local, calls) = StubReranker::build(StubBehaviour::Order(vec![ids[2], ids[1], ids[0]]));
    let out = fixture
        .stage_with_policy(&search, &config)
        .with_backends(Some(local), None)
        .with_policy(Rerank::CrossEncoder)
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

/// `search.rerank = "claude"` on a daemon whose AI subsystem never built a
/// provider is a degraded search, not a failed one.
#[tokio::test]
async fn an_unconfigured_backend_keeps_the_l1_order() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let stage = fixture
        .stage(&search)
        .with_backends(None, None)
        .with_policy(Rerank::Claude);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Deep,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

/// A backend that answers about candidates it was not given has told us
/// nothing about the ones it was — the answer is discarded whole rather
/// than partially applied.
#[tokio::test]
async fn a_backend_that_answers_about_other_candidates_is_discarded() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (stub, _) = StubReranker::build(StubBehaviour::Foreign);
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::CrossEncoder);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

/// A single-candidate page has no permutation to apply, so no backend is
/// paid to tell us so.
#[tokio::test]
async fn a_single_candidate_never_reaches_a_backend() {
    let fixture = Fixture::open().await;
    let (ids, _) = fixture.three().await;
    let ranked = vec![RankedCandidate {
        message_id: ids[0],
        score: 1.0,
    }];
    let search = fixture.search_config();
    let (stub, calls) = StubReranker::build(StubBehaviour::Flat);
    let stage = fixture
        .stage(&search)
        .with_backends(Some(stub), None)
        .with_policy(Rerank::CrossEncoder);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(ids_of(&out.ranked), vec![ids[0]]);
}

// ---------------------------------------------------------------------------
// The real Claude backend, against a mock provider
// ---------------------------------------------------------------------------

/// The whole listwise path: prompt assembly with positional labels,
/// redaction, the structured-output parse, the ordering, and prd.md's
/// per-result "why this matched".
#[tokio::test]
async fn claude_listwise_reorders_and_explains() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    // Labels are 1-based positions in the L1 order: prefer the second, then
    // the third, then the first.
    let (provider, calls) = MockProvider::new(vec![MockReply::Ok(listwise(&[2, 3, 1]))]);
    let stage = claude_stage(
        &fixture,
        Arc::clone(&provider) as Arc<dyn Provider>,
        &search,
    )
    .with_policy(Rerank::Claude);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Deep,
            &CancellationToken::new(),
        )
        .await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(out.backend, Some("claude"));
    assert_eq!(ids_of(&out.ranked), vec![ids[1], ids[2], ids[0]]);
    assert_eq!(
        out.reasons.get(&ids[1]).map(String::as_str),
        Some("reason 2")
    );
    assert_eq!(
        out.reasons.get(&ids[0]).map(String::as_str),
        Some("reason 1")
    );

    // Positional labels, never row ids — see `claude`'s module docs.
    let prompt = fixture_prompt(&provider);
    assert!(prompt.contains("[1]"), "prompt: {prompt}");
    assert!(prompt.contains("Invoice #338 Acme"), "prompt: {prompt}");
    assert!(
        !prompt.contains(&format!("id: {}", ids[0])),
        "the prompt must not carry local row ids"
    );
}

/// A model that ranks only some of the candidates does not produce a short
/// page: the ones it named keep its order, the rest follow in the order they
/// arrived.
#[tokio::test]
async fn an_incomplete_listwise_answer_appends_the_rest_in_l1_order() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (provider, _) = MockProvider::new(vec![MockReply::Ok(listwise(&[3]))]);
    let stage =
        claude_stage(&fixture, provider as Arc<dyn Provider>, &search).with_policy(Rerank::Claude);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Deep,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(ids_of(&out.ranked), vec![ids[2], ids[0], ids[1]]);
}

/// Labels the prompt never contained, and labels repeated twice, are both
/// ignored rather than trusted into a mis-attributed ordering.
#[tokio::test]
async fn out_of_range_and_repeated_labels_are_ignored() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let answer = serde_json::json!({
        "results": [
            {"label": 99, "why": "hallucinated"},
            {"label": 0, "why": "off by one"},
            {"label": 3, "why": "real"},
            {"label": 3, "why": "real again"},
        ]
    })
    .to_string();
    let (provider, _) = MockProvider::new(vec![MockReply::Ok(answer)]);
    let stage =
        claude_stage(&fixture, provider as Arc<dyn Provider>, &search).with_policy(Rerank::Claude);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Deep,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(ids_of(&out.ranked), vec![ids[2], ids[0], ids[1]]);
    assert_eq!(out.reasons.get(&ids[2]).map(String::as_str), Some("real"));
}

#[tokio::test]
async fn a_provider_failure_keeps_the_l1_order() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (provider, calls) = MockProvider::new(vec![MockReply::Fail]);
    let stage =
        claude_stage(&fixture, provider as Arc<dyn Provider>, &search).with_policy(Rerank::Claude);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Deep,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

/// A response that is not the schema's shape is a failed rerank, not a
/// partially-applied one.
#[tokio::test]
async fn a_malformed_listwise_answer_keeps_the_l1_order() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (provider, _) = MockProvider::new(vec![MockReply::Ok("not json at all".to_owned())]);
    let stage =
        claude_stage(&fixture, provider as Arc<dyn Provider>, &search).with_policy(Rerank::Claude);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Deep,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

/// prd.md: "Degrades to the L1 order on error/**budget**." A closed spend
/// cap must stop the call before it is made, not after it is paid for.
#[tokio::test]
async fn an_exhausted_budget_keeps_the_l1_order() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let mut search = fixture.search_config();
    search.rerank = Rerank::Claude;
    let (provider, calls) = MockProvider::new(vec![MockReply::Ok(listwise(&[3, 2, 1]))]);
    let limits = AiLimits {
        // Any spend at all is over this cap, and today's ledger already has
        // one call in it (recorded below), so the gate is closed.
        daily_cost_cap_usd: 0.000_001,
        ..AiLimits::default()
    };
    crate::ai::audit::record_call(
        &fixture.db,
        crate::ai::audit::CallRecord {
            account_id: None,
            message_id: None,
            request_id: None,
            model: "claude-haiku-4-5".to_owned(),
            pass: Some("triage".to_owned()),
            usage: Usage {
                input_tokens: 100_000,
                output_tokens: 100_000,
                ..Usage::default()
            },
            redaction_level: "none".to_owned(),
            latency: Duration::from_millis(1),
            payload: b"{}",
            outcome: crate::ai::audit::CallOutcome::Ok,
        },
    )
    .await
    .expect("seed a ledger entry");

    let claude: Arc<dyn Reranker> = Arc::new(ClaudeReranker::new(
        provider as Arc<dyn Provider>,
        fixture.db.clone(),
        &search.reranker,
        limits,
        AiPrivacy::default(),
        Arc::new(tokio::sync::Semaphore::new(4)),
        Arc::new(crate::ai::queue::RateLimiter::new(600)),
    ));
    let policy = Arc::new(PolicyEngine::from_config(&Config::default()).expect("valid ai policy"));
    let stage = L2Stage::new(fixture.db.clone(), &search, policy, Some(claude));

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Deep,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "an exhausted budget must stop the call before it is made"
    );
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// prd.md keys the cache on the candidate id *set*: the same messages
/// retrieved in a different L1 order are one entry, because a listwise
/// answer's whole job is to replace that order.
#[test]
fn cache_key_ignores_candidate_order() {
    let a = CacheKey::new("claude-haiku-4-5", "invoice", &[3, 1, 2]);
    let b = CacheKey::new("claude-haiku-4-5", "invoice", &[1, 2, 3]);
    assert_eq!(a, b);
    // ...and a repeated id is the same set.
    let c = CacheKey::new("claude-haiku-4-5", "invoice", &[1, 2, 3, 3]);
    assert_eq!(a, c);
}

/// Every other component of the key really is part of it — including the
/// boundary between the model and the query, which a naive concatenation
/// would blur.
#[test]
fn cache_key_separates_query_model_and_candidate_set() {
    let base = CacheKey::new("claude-haiku-4-5", "invoice", &[1, 2, 3]);
    assert_ne!(
        base,
        CacheKey::new("claude-haiku-4-5", "invoices", &[1, 2, 3]),
        "a different query must be a different key"
    );
    assert_ne!(
        base,
        CacheKey::new("claude-sonnet-5", "invoice", &[1, 2, 3]),
        "a different model must be a different key"
    );
    assert_ne!(
        base,
        CacheKey::new("claude-haiku-4-5", "invoice", &[1, 2, 4]),
        "a different candidate set must be a different key"
    );
    assert_ne!(
        base,
        CacheKey::new("claude-haiku-4-5", "invoice", &[1, 2]),
        "a subset must be a different key"
    );
    assert_ne!(
        CacheKey::new("ab", "c", &[1]),
        CacheKey::new("a", "bc", &[1]),
        "the model/query boundary must be part of the hash"
    );
}

#[test]
fn the_cache_evicts_least_recently_used_entries() {
    let cache = RerankCache::new(2);
    let a = CacheKey::new("m", "a", &[1]);
    let b = CacheKey::new("m", "b", &[1]);
    let c = CacheKey::new("m", "c", &[1]);
    let verdict = |id| {
        vec![RerankVerdict {
            message_id: id,
            score: 1.0,
            why: None,
        }]
    };
    cache.insert(a, verdict(1));
    cache.insert(b, verdict(2));
    // Touching `a` makes `b` the least recently used.
    assert!(cache.get(&a).is_some());
    cache.insert(c, verdict(3));
    assert_eq!(cache.len(), 2);
    assert!(cache.get(&a).is_some());
    assert!(cache.get(&c).is_some());
    assert!(cache.get(&b).is_none());
}

#[test]
fn a_zero_capacity_cache_stores_nothing() {
    let cache = RerankCache::new(0);
    let key = CacheKey::new("m", "q", &[1]);
    cache.insert(
        key,
        vec![RerankVerdict {
            message_id: 1,
            score: 1.0,
            why: None,
        }],
    );
    assert!(cache.get(&key).is_none());
    assert!(cache.is_empty());
}

/// The cache's reason for existing, end to end: a second identical query
/// (and a third whose candidates arrive in a different order) is served
/// without a second paid provider call, and returns the identical ordering.
#[tokio::test]
async fn the_listwise_cache_serves_a_repeat_query_without_a_provider_call() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    // Only one reply is queued: a second provider call would fail, which is
    // itself the assertion.
    let (provider, calls) = MockProvider::new(vec![MockReply::Ok(listwise(&[2, 3, 1]))]);
    let stage =
        claude_stage(&fixture, provider as Arc<dyn Provider>, &search).with_policy(Rerank::Claude);
    let cancel = CancellationToken::new();

    let first = stage
        .rerank("invoice", &ranked, SearchKind::Deep, &cancel)
        .await;
    assert_eq!(ids_of(&first.ranked), vec![ids[1], ids[2], ids[0]]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let second = stage
        .rerank("invoice", &ranked, SearchKind::Deep, &cancel)
        .await;
    assert_eq!(ids_of(&second.ranked), vec![ids[1], ids[2], ids[0]]);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "an identical query must be served from the cache"
    );
    assert_eq!(
        second.reasons.get(&ids[1]).map(String::as_str),
        Some("reason 2"),
        "a cached verdict keeps its per-result reason"
    );

    // The same candidates in a different L1 order: still one entry. The
    // labels the cached answer was written against are the *first* call's,
    // so the cached ordering is replayed by message id, not by position.
    let shuffled = vec![ranked[2], ranked[0], ranked[1]];
    let third = stage
        .rerank("invoice", &shuffled, SearchKind::Deep, &cancel)
        .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the same candidate set in a different order must hit the same entry"
    );
    assert_eq!(ids_of(&third.ranked), vec![ids[1], ids[2], ids[0]]);
}

/// A *different* candidate set is a different key, so it really does call
/// the provider again.
#[tokio::test]
async fn a_different_candidate_set_misses_the_cache() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let search = fixture.search_config();
    let (provider, calls) = MockProvider::new(vec![
        MockReply::Ok(listwise(&[2, 3, 1])),
        MockReply::Ok(listwise(&[2, 1])),
    ]);
    let stage =
        claude_stage(&fixture, provider as Arc<dyn Provider>, &search).with_policy(Rerank::Claude);
    let cancel = CancellationToken::new();

    let _ = stage
        .rerank("invoice", &ranked, SearchKind::Deep, &cancel)
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let narrower = vec![ranked[0], ranked[1]];
    let out = stage
        .rerank("invoice", &narrower, SearchKind::Deep, &cancel)
        .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(ids_of(&out.ranked), vec![ids[1], ids[0]]);
}

// ---------------------------------------------------------------------------
// The ONNX backend's reachable behaviour
// ---------------------------------------------------------------------------

/// The task's rule for the model file: never vendored, path configurable,
/// and an absent model degrades with a message that says how to fix it —
/// naming the config key and the cache env var, not just "failed".
#[tokio::test]
async fn an_unprovisioned_cross_encoder_says_how_to_fix_it() {
    let empty = std::env::temp_dir().join(format!(
        "rmail-rank-l2-no-model-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let config = RerankerConfig {
        cross_encoder_cache_dir: empty.display().to_string(),
        cross_encoder_allow_download: false,
        ..RerankerConfig::default()
    };
    let backend = CrossEncoderReranker::new(&config);
    let candidates = vec![RerankCandidate {
        message_id: 1,
        document: "Subject: anything".to_owned(),
    }];
    let error = backend
        .rerank("invoice", &candidates, &CancellationToken::new())
        .await
        .expect_err("an unprovisioned model cannot rerank");
    let message = error.to_string();
    #[cfg(feature = "onnx")]
    assert!(
        message.contains("cross_encoder_allow_download") && message.contains("RMAIL_MODEL_CACHE"),
        "the message must name the fix, got: {message}"
    );
    #[cfg(not(feature = "onnx"))]
    assert!(
        message.contains("onnx"),
        "the message must name the missing feature, got: {message}"
    );
    assert!(
        message.contains("L1 order"),
        "the message must say what the user gets instead, got: {message}"
    );
}

/// An unknown model id is caught with a list of the real ones rather than
/// failing somewhere inside a model loader.
#[cfg(feature = "onnx")]
#[tokio::test]
async fn an_unknown_cross_encoder_model_names_the_supported_ones() {
    let config = RerankerConfig {
        cross_encoder_model: "definitely-not-a-model".to_owned(),
        cross_encoder_allow_download: true,
        ..RerankerConfig::default()
    };
    let backend = CrossEncoderReranker::new(&config);
    let candidates = vec![RerankCandidate {
        message_id: 1,
        document: "Subject: anything".to_owned(),
    }];
    let error = backend
        .rerank("invoice", &candidates, &CancellationToken::new())
        .await
        .expect_err("an unknown model id cannot rerank");
    assert!(
        error.to_string().contains("bge-reranker-base"),
        "got: {error}"
    );
}

/// The ONNX backend is wired into the stage the same way the stub is, and
/// an unprovisioned model degrades through the *stage* rather than only
/// through the backend — the path a real deployment takes.
#[tokio::test]
async fn an_unprovisioned_cross_encoder_degrades_through_the_stage() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let mut search = fixture.search_config();
    search.reranker.cross_encoder_cache_dir = std::env::temp_dir()
        .join(format!(
            "rmail-rank-l2-no-model-stage-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
        .display()
        .to_string();
    // The real backend, not a stub: `L2Stage::new` always builds it.
    let stage = fixture.stage(&search).with_policy(Rerank::CrossEncoder);

    let out = stage
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Interactive,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

/// `L2Stage::disabled()` is a real off switch, not a stage with no backends
/// that still pays for a document fetch.
#[tokio::test]
async fn a_disabled_stage_is_a_passthrough() {
    let fixture = Fixture::open().await;
    let (ids, ranked) = fixture.three().await;
    let out = L2Stage::disabled()
        .rerank(
            "invoice",
            &ranked,
            SearchKind::Deep,
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(out.backend, None);
    assert_eq!(ids_of(&out.ranked), ids);
}

fn fixture_prompt(provider: &MockProvider) -> String {
    provider.prompts().join("\n")
}
