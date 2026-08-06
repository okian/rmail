//! The properties this module exists for: dedup, a lease a reaped owner can
//! no longer complete, `Semaphore(max_concurrency)` actually bounding
//! in-flight calls, the RPM limiter pacing rather than bursting, the cost
//! gate blocking *before* the provider is ever called, the batch flip
//! firing at threshold (and not before) with `custom_id = message_id`, and
//! provider failures backing off then quarantining to `dead` with
//! `mail ai retry --failed`'s `revive_all_dead` bringing them back.
//!
//! Every test drives the real pipeline (`AiWorkerPool`/`BatchCoordinator`)
//! against a hand-rolled [`MockProvider`] and — for the batch tests — a
//! real HTTP server on loopback ([`MockHttp`]), the same "test against a
//! socket, not a mocked client" discipline `ai::provider`'s own tests use.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ai::policy::PolicyEngine;
use crate::ai::provider::{ChatResponse, Provider, ProviderStream, StopReason, Usage};
use crate::ai::redact::{self, GuardedRequest};
use crate::config::{AiBatching, AiPolicyMode, AiPolicyRule, AiPrivacy};
use crate::repo;
use crate::ErrorReason;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    db: Database,
    queue: AiQueue,
    path: PathBuf,
    account_id: i64,
    inbox_id: i64,
    legal_id: i64,
    next_uid: AtomicI64,
}

impl Fixture {
    async fn open() -> Self {
        Self::with_options(QueueOptions::default()).await
    }

    async fn with_options(opts: QueueOptions) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-aiq-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let (account_id, inbox_id, legal_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let inbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                // A folder an `ai.policy` rule marks `forbidden` — see
                // `open_policy`.
                let legal_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "Legal".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox_id, legal_id))
            })
            .await
            .unwrap();
        let queue = AiQueue::new(db.clone(), opts);
        Self {
            db,
            queue,
            path,
            account_id,
            inbox_id,
            legal_id,
            next_uid: AtomicI64::new(1),
        }
    }

    /// A message in `INBOX` with `body_text` set to `body`.
    async fn message(&self, body: &str) -> i64 {
        self.message_in(self.inbox_id, body).await
    }

    async fn message_in(&self, mailbox_id: i64, body: &str) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let account_id = self.account_id;
        let body = body.to_owned();
        self.db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some("Test message".to_owned()),
                        body_text: Some(body),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    /// Force a lease into the past, standing in for the worker having died —
    /// the same trick `index::queue`'s own tests use.
    async fn expire_lease(&self, job_id: i64) {
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE ai_queue SET lease_expires_at = 1 WHERE job_id = ?1",
                    [job_id],
                )
            })
            .await
            .unwrap();
    }

    /// Seed today's `ai_usage` rollup directly — the cost gate's input —
    /// without going through a real `record_call`, so cap tests can set an
    /// exact number rather than working backward from pricing.
    async fn seed_today_usage(&self, cost_usd: f64, tokens: i64) {
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO ai_usage (day, requests, input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, cost_usd)
                     VALUES (?1, 1, ?2, 0, 0, 0, ?3)
                     ON CONFLICT(day) DO UPDATE SET
                         input_tokens = excluded.input_tokens, cost_usd = excluded.cost_usd",
                    rusqlite::params![day, tokens, cost_usd],
                )
            })
            .await
            .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// A policy engine with one rule: the `Legal` folder (any account) is
/// forbidden. Everything else defaults `Allowed`.
fn open_policy() -> PolicyEngine {
    PolicyEngine::new(
        vec![AiPolicyRule {
            account: None,
            folder: Some("Legal".to_owned()),
            mode: AiPolicyMode::Forbidden,
            residency: None,
            reason: None,
        }],
        AiPolicyMode::Allowed,
        "unspecified",
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// A recording PassHandler
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct RecordingHandler {
    pass: String,
    successes: Mutex<Vec<(i64, String, i64)>>,
}

impl RecordingHandler {
    fn new(pass: &str) -> Self {
        Self {
            pass: pass.to_owned(),
            successes: Mutex::new(Vec::new()),
        }
    }

    fn successes(&self) -> Vec<(i64, String, i64)> {
        self.successes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl PassHandler for RecordingHandler {
    fn pass(&self) -> &str {
        &self.pass
    }

    fn build_request(&self, content: &MessageContent) -> Result<ChatRequest, Error> {
        Ok(ChatRequest::new("mock-model", 256)
            .system("You are a test fixture.")
            .user(content.body.clone()))
    }

    async fn on_success(
        &self,
        lease: &AiLease,
        text: &str,
        ledger_entry_id: i64,
    ) -> Result<(), Error> {
        self.successes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((lease.message_id, text.to_owned(), ledger_entry_id));
        Ok(())
    }
}

fn triage_handler() -> Arc<RecordingHandler> {
    Arc::new(RecordingHandler::new("triage"))
}

fn build_pool(
    fx: &Fixture,
    provider: MockProvider,
    limits: AiLimits,
    handlers: Vec<Arc<dyn PassHandler>>,
) -> AiWorkerPool {
    let provider: Arc<dyn Provider> = Arc::new(provider);
    AiWorkerPool::new(
        fx.db.clone(),
        fx.queue.clone(),
        provider,
        Arc::new(open_policy()),
        limits,
        AiPrivacy::default(),
        handlers,
        "test-worker",
    )
}

fn high_rpm_limits() -> AiLimits {
    AiLimits {
        max_concurrency: 8,
        requests_per_minute: 1_000_000,
        ..AiLimits::default()
    }
}

// ---------------------------------------------------------------------------
// A mock Provider
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum MockReply {
    Ok(String),
    Unavailable,
}

#[derive(Debug)]
struct MockProvider {
    replies: Mutex<VecDeque<MockReply>>,
    delay: Duration,
    calls: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    peak_in_flight: Arc<AtomicUsize>,
}

/// Shared counters a test keeps after handing the [`MockProvider`] itself
/// (as `Arc<dyn Provider>`) into a pool.
#[derive(Debug, Clone)]
struct MockProviderHandle {
    calls: Arc<AtomicUsize>,
    peak_in_flight: Arc<AtomicUsize>,
}

impl MockProviderHandle {
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::SeqCst)
    }
}

impl MockProvider {
    fn new(replies: Vec<MockReply>) -> (Self, MockProviderHandle) {
        let calls = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak_in_flight = Arc::new(AtomicUsize::new(0));
        let provider = Self {
            replies: Mutex::new(replies.into()),
            delay: Duration::ZERO,
            calls: calls.clone(),
            in_flight,
            peak_in_flight: peak_in_flight.clone(),
        };
        (
            provider,
            MockProviderHandle {
                calls,
                peak_in_flight,
            },
        )
    }

    #[must_use]
    fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _request: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_in_flight.fetch_max(current, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::select! {
                () = tokio::time::sleep(self.delay) => {}
                () = cancel.cancelled() => {}
            }
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
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
            Some(MockReply::Unavailable) | None => {
                Err(Error::unavailable("mock provider unavailable".to_owned()))
            }
        }
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        Err(Error::unavailable(
            "mock provider does not implement streaming".to_owned(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Queue mechanics: dedup, lease reclaim, revive
// ---------------------------------------------------------------------------

#[tokio::test]
async fn enqueue_dedups_on_message_and_pass() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;

    let first = fx
        .queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();
    let second = fx
        .queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    assert_eq!(first, 1);
    assert_eq!(
        second, 0,
        "the same (message_id, pass) must not queue twice"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.ready, 1);

    // A different pass on the same message is a different job.
    let deep = fx
        .queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "deep")])
        .await
        .unwrap();
    assert_eq!(deep, 1);
}

#[tokio::test]
async fn lease_reclaimed_after_expiry_cannot_be_completed_by_original_owner() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let original = fx
        .queue
        .lease("worker-a", 1, None)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    fx.expire_lease(original.job_id).await;
    let reclaimed = fx.queue.reap_expired().await.unwrap();
    assert_eq!(reclaimed, 1);

    let new_owner = fx
        .queue
        .lease("worker-b", 1, None)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(new_owner.job_id, original.job_id);
    assert_eq!(new_owner.worker, "worker-b");

    let held_by_original = fx.queue.complete(&original, None).await.unwrap();
    assert!(
        !held_by_original,
        "the original (now-stale) owner must not be able to complete a reclaimed lease"
    );
    let held_by_new_owner = fx.queue.complete(&new_owner, None).await.unwrap();
    assert!(held_by_new_owner, "the new owner's completion must succeed");

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.done, 1);
}

#[tokio::test]
async fn ai_queue_pending_rows_persist_across_instances() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    // A fresh `AiQueue` over the same database (standing in for a daemon
    // restart, or reconnecting after being offline) sees the same pending
    // row — nothing about durability depends on the process that enqueued
    // it still running.
    let reopened = AiQueue::new(fx.db.clone(), QueueOptions::default());
    let stats = reopened.stats().await.unwrap();
    assert_eq!(stats.ready, 1);
}

#[tokio::test]
async fn transient_failures_back_off_then_quarantine_and_revive_all_dead_requeues() {
    let opts = QueueOptions {
        max_attempts: 2,
        backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(2),
        lease: Duration::from_secs(300),
    };
    let fx = Fixture::with_options(opts).await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let lease1 = fx.queue.lease("w", 1, None).await.unwrap().remove(0);
    let outcome1 = fx.queue.fail(&lease1, "429").await.unwrap();
    assert!(matches!(
        outcome1,
        Some(Failure::Retrying { attempts: 1, .. })
    ));

    tokio::time::sleep(Duration::from_millis(10)).await;
    let lease2 = fx.queue.lease("w", 1, None).await.unwrap().remove(0);
    let outcome2 = fx.queue.fail(&lease2, "429 again").await.unwrap();
    assert!(matches!(
        outcome2,
        Some(Failure::Quarantined { attempts: 2 })
    ));

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.dead, 1);
    let dead = fx.queue.dead_letters(10).await.unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].last_error.as_deref(), Some("429 again"));

    // `mail ai retry --failed`.
    let revived = fx.queue.revive_all_dead().await.unwrap();
    assert_eq!(revived, 1);
    let stats_after = fx.queue.stats().await.unwrap();
    assert_eq!(stats_after.ready, 1);
    assert_eq!(stats_after.dead, 0);
}

// ---------------------------------------------------------------------------
// The worker pool: concurrency, pacing, cost gate, redaction, policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispatch_pending_bounds_concurrency_to_max_concurrency() {
    let fx = Fixture::open().await;
    let n = 6;
    let mut jobs = Vec::new();
    for i in 0..n {
        let id = fx
            .message(&format!("message {i} about quarterly numbers"))
            .await;
        jobs.push(NewAiJob::new(id, fx.account_id, "triage"));
    }
    fx.queue.enqueue(jobs).await.unwrap();

    let (provider, handle) = MockProvider::new(vec![MockReply::Ok("summary".to_owned()); n]);
    let provider = provider.with_delay(Duration::from_millis(150));
    let limits = AiLimits {
        max_concurrency: 2,
        requests_per_minute: 1_000_000,
        ..AiLimits::default()
    };
    let handler = triage_handler();
    let pool = build_pool(&fx, provider, limits, vec![handler]);
    let cancel = CancellationToken::new();

    let summary = pool.dispatch_pending(n as i64, &cancel).await.unwrap();
    assert_eq!(summary.completed, n as u64);
    assert!(
        handle.peak_in_flight() <= 2,
        "peak in-flight {} exceeded max_concurrency = 2",
        handle.peak_in_flight()
    );
    assert!(
        handle.peak_in_flight() >= 2,
        "test never actually exercised concurrency (peak was {}); \
         it would pass even with a broken semaphore",
        handle.peak_in_flight()
    );
}

#[tokio::test]
async fn rate_limiter_paces_rather_than_bursts() {
    // 10 tokens/sec once the single starting token is spent: acquiring four
    // in a row should take roughly 300ms (three ~100ms waits), not ~0ms.
    let limiter = RateLimiter::new(600);
    let start = Instant::now();
    let mut timestamps = Vec::with_capacity(4);
    for _ in 0..4 {
        limiter.acquire().await;
        timestamps.push(Instant::now());
    }
    for pair in timestamps.windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        assert!(
            gap >= Duration::from_millis(70),
            "acquired without pacing: consecutive calls only {gap:?} apart"
        );
    }
    assert!(
        start.elapsed() >= Duration::from_millis(210),
        "four acquires paced at ~100ms should take at least ~300ms, took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn cost_gate_pause_blocks_dispatch_before_provider_is_called() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();
    fx.seed_today_usage(1000.0, 0).await; // far over the default $5/day cap

    let (provider, handle) = MockProvider::new(vec![MockReply::Ok("x".to_owned())]);
    let mut limits = high_rpm_limits();
    limits.on_cap = OnCap::Pause;
    let pool = build_pool(&fx, provider, limits, vec![triage_handler()]);

    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();
    assert!(summary.paused);
    assert_eq!(
        handle.call_count(),
        0,
        "the provider must never be called while the cost gate is paused"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(
        stats.ready, 1,
        "the job must remain pending, not leased or lost"
    );
}

#[tokio::test]
async fn cost_gate_triage_only_admits_only_the_triage_pass() {
    let fx = Fixture::open().await;
    let triage_id = fx.message("triage me, nothing sensitive here").await;
    let deep_id = fx.message("deep pass me, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![
            NewAiJob::new(triage_id, fx.account_id, "triage"),
            NewAiJob::new(deep_id, fx.account_id, "deep"),
        ])
        .await
        .unwrap();
    fx.seed_today_usage(1000.0, 0).await;

    let (provider, handle) = MockProvider::new(vec![MockReply::Ok("summary".to_owned())]);
    let mut limits = high_rpm_limits();
    limits.on_cap = OnCap::TriageOnly;
    let pool = build_pool(
        &fx,
        provider,
        limits,
        vec![triage_handler(), Arc::new(RecordingHandler::new("deep"))],
    );

    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(summary.completed, 1);
    assert_eq!(
        handle.call_count(),
        1,
        "only the triage job may reach the provider"
    );

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.done, 1, "the triage job completed");
    assert_eq!(stats.ready, 1, "the deep job was held back, not lost");
}

#[tokio::test]
async fn cost_gate_drop_terminates_without_calling_provider() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();
    fx.seed_today_usage(1000.0, 0).await;

    let (provider, handle) = MockProvider::new(vec![MockReply::Ok("x".to_owned())]);
    let mut limits = high_rpm_limits();
    limits.on_cap = OnCap::Drop;
    let pool = build_pool(&fx, provider, limits, vec![triage_handler()]);

    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(summary.dropped, 1);
    assert_eq!(
        handle.call_count(),
        0,
        "a dropped job must never reach the provider, only be leased and terminated"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.error, 1);
}

#[tokio::test]
async fn redacted_skip_terminates_without_calling_provider() {
    let fx = Fixture::open().await;
    // A body that is entirely PII: once redacted there is nothing left.
    let id = fx.message("only-contact@example.com").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let (provider, handle) = MockProvider::new(vec![MockReply::Ok("x".to_owned())]);
    let pool = build_pool(&fx, provider, high_rpm_limits(), vec![triage_handler()]);
    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(summary.terminated, 1);
    assert_eq!(
        handle.call_count(),
        0,
        "redacted_skip must never call the provider"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.error, 1);
}

#[tokio::test]
async fn policy_forbidden_terminates_without_calling_provider() {
    let fx = Fixture::open().await;
    let id = fx
        .message_in(fx.legal_id, "some correspondence, nothing else notable")
        .await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let (provider, handle) = MockProvider::new(vec![MockReply::Ok("x".to_owned())]);
    let pool = build_pool(&fx, provider, high_rpm_limits(), vec![triage_handler()]);
    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(summary.terminated, 1);
    assert_eq!(
        handle.call_count(),
        0,
        "a policy-forbidden folder must never reach the provider"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.error, 1);
}

#[tokio::test]
async fn dispatch_pending_quarantines_after_provider_failures_and_revive_requeues() {
    let opts = QueueOptions {
        max_attempts: 1,
        ..QueueOptions::default()
    };
    let fx = Fixture::with_options(opts).await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let (provider, handle) = MockProvider::new(vec![MockReply::Unavailable]);
    let pool = build_pool(&fx, provider, high_rpm_limits(), vec![triage_handler()]);
    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(summary.dead, 1);
    assert_eq!(handle.call_count(), 1);
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.dead, 1);

    let revived = fx.queue.revive_all_dead().await.unwrap();
    assert_eq!(revived, 1);
}

#[tokio::test]
async fn dispatch_pending_persists_the_rehydrated_response_via_the_handler() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let (provider, _handle) =
        MockProvider::new(vec![MockReply::Ok("here is your summary".to_owned())]);
    let handler = triage_handler();
    let pool = build_pool(
        &fx,
        provider,
        high_rpm_limits(),
        vec![handler.clone() as Arc<dyn PassHandler>],
    );
    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(summary.completed, 1);

    let successes = handler.successes();
    assert_eq!(successes.len(), 1);
    assert_eq!(successes[0].0, id);
    assert_eq!(successes[0].1, "here is your summary");
    assert!(
        successes[0].2 > 0,
        "a ledger entry id must have been recorded"
    );

    let calls =
        crate::ai::audit::query_calls(&fx.db, &crate::ai::audit::AuditFilter::default(), 10, None)
            .await
            .unwrap();
    assert_eq!(
        calls.len(),
        1,
        "the redacted call must be in the audit ledger"
    );
    assert_eq!(calls[0].status, crate::ai::audit::CallStatus::Ok);
}

// ---------------------------------------------------------------------------
// Batch mode: a real HTTP server on loopback
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    path: String,
    body: serde_json::Value,
}

struct MockHttp {
    endpoint: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl MockHttp {
    /// Answers connections in order from `replies` (status, body); once
    /// exhausted, further connections get a 500 — enough to make an
    /// unexpected extra call to fail loudly rather than silently reuse the
    /// last response.
    async fn queued(replies: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                let reply = {
                    let mut queue = replies.lock().unwrap_or_else(PoisonError::into_inner);
                    queue
                        .pop_front()
                        .unwrap_or((500, "{\"error\":\"no reply queued\"}".to_owned()))
                };
                tokio::spawn(handle_connection(stream, recorder, reply));
            }
        });
        Self {
            endpoint: format!("http://{addr}"),
            seen,
            task,
        }
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().map(|log| log.clone()).unwrap_or_default()
    }
}

impl Drop for MockHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    recorder: Arc<Mutex<Vec<SeenRequest>>>,
    reply: (u16, String),
) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let Some((head_end, length, method, path)) =
        read_request_head(&mut stream, &mut raw, &mut buf).await
    else {
        return;
    };
    let body_text = String::from_utf8_lossy(&raw[head_end..head_end + length]).to_string();
    let body = if body_text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&body_text).unwrap_or(serde_json::Value::Null)
    };
    if let Ok(mut log) = recorder.lock() {
        log.push(SeenRequest { method, path, body });
    }
    let (status, resp_body) = reply;
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
        resp_body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

async fn read_request_head(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    buf: &mut [u8; 4096],
) -> Option<(usize, usize, String, String)> {
    loop {
        let n = stream.read(buf).await.unwrap_or(0);
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(raw).to_string();
        if let Some(at) = text.find("\r\n\r\n") {
            let length = header_value(&text, "content-length")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() >= at + 4 + length {
                let first_line = text.lines().next().unwrap_or("");
                let mut parts = first_line.split_whitespace();
                let method = parts.next().unwrap_or("GET").to_owned();
                let path = parts.next().unwrap_or("/").to_owned();
                return Some((at + 4, length, method, path));
            }
        }
    }
}

fn header_value(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

fn coordinator(
    fx: &Fixture,
    endpoint: &str,
    batching: AiBatching,
    handlers: Vec<Arc<dyn PassHandler>>,
) -> BatchCoordinator {
    coordinator_with_limits(fx, endpoint, high_rpm_limits(), batching, handlers)
}

fn coordinator_with_limits(
    fx: &Fixture,
    endpoint: &str,
    limits: AiLimits,
    batching: AiBatching,
    handlers: Vec<Arc<dyn PassHandler>>,
) -> BatchCoordinator {
    let client = BatchClient::new().unwrap().with_endpoint(endpoint);
    BatchCoordinator::new(
        fx.db.clone(),
        fx.queue.clone(),
        client,
        "printf secret-key",
        Arc::new(open_policy()),
        limits,
        AiPrivacy::default(),
        batching,
        handlers,
    )
    .unwrap()
}

#[tokio::test]
async fn batch_flip_submits_at_threshold_with_custom_id_eq_message_id() {
    let fx = Fixture::open().await;
    let mut ids = Vec::new();
    for i in 0..3 {
        ids.push(
            fx.message(&format!("message {i}, nothing sensitive here"))
                .await,
        );
    }
    fx.queue
        .enqueue(
            ids.iter()
                .map(|&id| NewAiJob::new(id, fx.account_id, "triage"))
                .collect(),
        )
        .await
        .unwrap();

    let submit_body = serde_json::json!({
        "id": "batch_123",
        "processing_status": "in_progress",
    })
    .to_string();
    let http = MockHttp::queued(vec![(200, submit_body)]).await;
    let handler: Arc<dyn PassHandler> = triage_handler();
    let coord = coordinator(
        &fx,
        &http.endpoint,
        AiBatching {
            enabled: true,
            threshold: 3,
            max_batch: 10,
        },
        vec![handler],
    );

    let batch_id = coord.maybe_submit("triage").await.unwrap();
    assert_eq!(batch_id.as_deref(), Some("batch_123"));

    let requests = http.requests();
    assert_eq!(
        requests.len(),
        1,
        "exactly one batch submission is expected"
    );
    assert_eq!(requests[0].method, "POST");
    assert_eq!(
        requests[0].path, "/",
        "submit posts to the base batches endpoint"
    );
    let sent: Vec<String> = requests[0].body["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["custom_id"].as_str().unwrap().to_owned())
        .collect();
    let mut sent_sorted = sent.clone();
    sent_sorted.sort();
    let mut expected: Vec<String> = ids.iter().map(i64::to_string).collect();
    expected.sort();
    assert_eq!(
        sent_sorted, expected,
        "custom_id must equal the message id for every item"
    );

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(
        stats.leased, 3,
        "batched jobs stay leased pending the batch's eventual result"
    );
}

#[tokio::test]
async fn batch_does_not_flip_below_threshold() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let http = MockHttp::queued(vec![]).await;
    let handler: Arc<dyn PassHandler> = triage_handler();
    let coord = coordinator(
        &fx,
        &http.endpoint,
        AiBatching {
            enabled: true,
            threshold: 5,
            max_batch: 10,
        },
        vec![handler],
    );

    let result = coord.maybe_submit("triage").await.unwrap();
    assert_eq!(result, None);
    assert!(
        http.requests().is_empty(),
        "no HTTP call should be made while depth is below threshold"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.ready, 1, "the job is untouched, still pending live");
}

#[tokio::test]
async fn batch_poll_completes_succeeded_and_backs_off_errored_items() {
    let fx = Fixture::open().await;
    let id_ok = fx.message("hello, nothing sensitive here").await;
    let id_err = fx
        .message("a second message, also nothing sensitive here")
        .await;
    fx.queue
        .enqueue(vec![
            NewAiJob::new(id_ok, fx.account_id, "triage"),
            NewAiJob::new(id_err, fx.account_id, "triage"),
        ])
        .await
        .unwrap();

    let submit_body = serde_json::json!({
        "id": "batch_xyz",
        "processing_status": "in_progress",
    })
    .to_string();
    let status_body = serde_json::json!({
        "id": "batch_xyz",
        "processing_status": "ended",
        "request_counts": {"processing": 0, "succeeded": 1, "errored": 1, "canceled": 0, "expired": 0},
    })
    .to_string();
    let results_body = format!(
        "{}\n{}\n",
        serde_json::json!({
            "custom_id": id_ok.to_string(),
            "result": {
                "type": "succeeded",
                "message": {
                    "id": "msg_1",
                    "model": "claude-haiku-4-5",
                    "content": [{"type": "text", "text": "summary text"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
                },
            },
        }),
        serde_json::json!({
            "custom_id": id_err.to_string(),
            "result": {"type": "errored", "error": {"type": "invalid_request", "message": "bad input"}},
        }),
    );
    let http = MockHttp::queued(vec![
        (200, submit_body),
        (200, status_body),
        (200, results_body),
    ])
    .await;
    let handler = triage_handler();
    let coord = coordinator(
        &fx,
        &http.endpoint,
        AiBatching {
            enabled: true,
            threshold: 2,
            max_batch: 10,
        },
        vec![handler.clone() as Arc<dyn PassHandler>],
    );

    let batch_id = coord.maybe_submit("triage").await.unwrap().unwrap();

    let outcome = coord.poll(&batch_id).await.unwrap();
    let requests = http.requests();
    assert_eq!(requests.len(), 3, "submit, status, and results");
    assert_eq!(requests[1].path, format!("/{batch_id}"));
    assert_eq!(requests[2].path, format!("/{batch_id}/results"));
    let BatchPollOutcome::Completed(summary) = outcome else {
        unreachable!("expected the batch to have ended, got {outcome:?}");
    };
    assert_eq!(summary.completed, 1);
    assert_eq!(
        summary.retried, 1,
        "an errored item backs off like any transient failure"
    );

    let successes = handler.successes();
    assert_eq!(successes.len(), 1);
    assert_eq!(successes[0].0, id_ok);
    assert_eq!(successes[0].1, "summary text");

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.done, 1);
    assert_eq!(stats.backing_off, 1);
}

#[tokio::test]
async fn batch_poll_still_running_leaves_jobs_leased() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let submit_body =
        serde_json::json!({"id": "batch_running", "processing_status": "in_progress"}).to_string();
    let status_body = serde_json::json!({
        "id": "batch_running",
        "processing_status": "in_progress",
        "request_counts": {"processing": 1, "succeeded": 0, "errored": 0, "canceled": 0, "expired": 0},
    })
    .to_string();
    let http = MockHttp::queued(vec![(200, submit_body), (200, status_body)]).await;
    let handler: Arc<dyn PassHandler> = triage_handler();
    let coord = coordinator(
        &fx,
        &http.endpoint,
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![handler],
    );

    let batch_id = coord.maybe_submit("triage").await.unwrap().unwrap();
    let outcome = coord.poll(&batch_id).await.unwrap();
    assert_eq!(outcome, BatchPollOutcome::StillRunning);

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.leased, 1, "still-running jobs stay leased, untouched");
}

// ---------------------------------------------------------------------------
// Regression tests from review: the cost gate must also cover the batch
// path, a failed submission must not strand leases, redaction must
// demonstrably reach both the ledger and the batch wire body, a batch
// result must be ledgered at half price, and a cancellation must not
// silently cost a job a retry attempt.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn batch_maybe_submit_respects_pause_and_never_leases_or_submits() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();
    fx.seed_today_usage(1000.0, 0).await;

    let http = MockHttp::queued(vec![]).await;
    let mut limits = high_rpm_limits();
    limits.on_cap = OnCap::Pause;
    let coord = coordinator_with_limits(
        &fx,
        &http.endpoint,
        limits,
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![triage_handler() as Arc<dyn PassHandler>],
    );

    let result = coord.maybe_submit("triage").await.unwrap();
    assert_eq!(result, None);
    assert!(
        http.requests().is_empty(),
        "no HTTP call may be made while the cost gate is paused"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(
        stats.ready, 1,
        "the job must stay pending, not leased, while paused"
    );
}

#[tokio::test]
async fn batch_maybe_submit_respects_triage_only() {
    let fx = Fixture::open().await;
    let deep_id = fx.message("deep pass me, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(deep_id, fx.account_id, "deep")])
        .await
        .unwrap();
    fx.seed_today_usage(1000.0, 0).await;

    let http = MockHttp::queued(vec![]).await;
    let mut limits = high_rpm_limits();
    limits.on_cap = OnCap::TriageOnly;
    let coord = coordinator_with_limits(
        &fx,
        &http.endpoint,
        limits,
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![Arc::new(RecordingHandler::new("deep")) as Arc<dyn PassHandler>],
    );

    let result = coord.maybe_submit("deep").await.unwrap();
    assert_eq!(result, None);
    assert!(
        http.requests().is_empty(),
        "a non-triage pass must not submit while capped to triage_only"
    );
}

#[tokio::test]
async fn batch_maybe_submit_respects_drop_and_terminates_without_submitting() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();
    fx.seed_today_usage(1000.0, 0).await;

    let http = MockHttp::queued(vec![]).await;
    let mut limits = high_rpm_limits();
    limits.on_cap = OnCap::Drop;
    let coord = coordinator_with_limits(
        &fx,
        &http.endpoint,
        limits,
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![triage_handler() as Arc<dyn PassHandler>],
    );

    let result = coord.maybe_submit("triage").await.unwrap();
    assert_eq!(result, None);
    assert!(
        http.requests().is_empty(),
        "a dropped batch backlog must never reach the submit endpoint"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(
        stats.error, 1,
        "the job must be terminated, not left pending or leased"
    );
}

#[tokio::test]
async fn batch_submit_failure_returns_leases_to_pending_rather_than_stranding_them() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    // A 500 on the submit call itself.
    let http = MockHttp::queued(vec![(500, "{\"error\":\"boom\"}".to_owned())]).await;
    let coord = coordinator(
        &fx,
        &http.endpoint,
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![triage_handler() as Arc<dyn PassHandler>],
    );

    let result = coord.maybe_submit("triage").await;
    assert!(result.is_err(), "the submit failure must propagate");

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(
        stats.leased, 0,
        "a failed submission must not leave the job leased for the full batch TTL"
    );
    assert_eq!(
        stats.outstanding(),
        1,
        "the job must still be outstanding (pending/backing off), not lost"
    );
}

#[tokio::test]
async fn dispatch_pending_audits_the_redacted_payload_not_the_raw_one() {
    let fx = Fixture::open().await;
    let body = "please review the invoice from finance-team@example.com and let me know";
    let id = fx.message(body).await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let (provider, _handle) = MockProvider::new(vec![MockReply::Ok("ok".to_owned())]);
    let handler = triage_handler();
    let pool = build_pool(
        &fx,
        provider,
        high_rpm_limits(),
        vec![handler.clone() as Arc<dyn PassHandler>],
    );
    let summary = pool
        .dispatch_pending(10, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(summary.completed, 1);

    // Reconstruct, independently, what the raw and redacted payloads each
    // hash to — a probe: if `process_one` ever audited `request` instead of
    // `redacted_request`, this test must fail.
    let content = assemble_content(&fx.db, id, &AiPrivacy::default())
        .await
        .unwrap();
    let raw_request = handler.build_request(&content).unwrap();
    let raw_hash = Sha256::digest(payload_bytes(&raw_request)).to_vec();
    let redacted_request = match redact::guard(&raw_request, &AiPrivacy::default()) {
        GuardedRequest::Redacted { request, .. } => request,
        GuardedRequest::RedactedSkip => unreachable!("this body has residual content"),
    };
    let redacted_hash = Sha256::digest(payload_bytes(&redacted_request)).to_vec();
    assert_ne!(
        raw_hash, redacted_hash,
        "sanity check: redaction must actually change the payload for this body"
    );

    let calls = audit::query_calls(&fx.db, &audit::AuditFilter::default(), 10, None)
        .await
        .unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].payload_sha256, redacted_hash,
        "the ledger must hash the redacted payload"
    );
    assert_ne!(
        calls[0].payload_sha256, raw_hash,
        "the ledger must never hash the raw, unredacted payload"
    );
}

#[tokio::test]
async fn batch_submission_sends_the_redacted_body_not_the_raw_one() {
    let fx = Fixture::open().await;
    let id = fx
        .message("please review the invoice from finance-team@example.com and let me know")
        .await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let submit_body =
        serde_json::json!({"id": "batch_pii", "processing_status": "in_progress"}).to_string();
    let http = MockHttp::queued(vec![(200, submit_body)]).await;
    let coord = coordinator(
        &fx,
        &http.endpoint,
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![triage_handler() as Arc<dyn PassHandler>],
    );
    coord.maybe_submit("triage").await.unwrap();

    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    let sent_text = requests[0].body["requests"][0]["params"]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        !sent_text.contains("finance-team@example.com"),
        "raw PII must never be sent to the batch endpoint; got: {sent_text}"
    );
    assert!(
        sent_text.contains("EMAIL_1"),
        "the redacted token should be present instead; got: {sent_text}"
    );
}

#[tokio::test]
async fn batch_poll_ledgers_a_succeeded_result_at_half_the_live_price() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let submit_body =
        serde_json::json!({"id": "batch_priced", "processing_status": "in_progress"}).to_string();
    let status_body = serde_json::json!({
        "id": "batch_priced",
        "processing_status": "ended",
        "request_counts": {"processing": 0, "succeeded": 1, "errored": 0, "canceled": 0, "expired": 0},
    })
    .to_string();
    let usage = serde_json::json!({
        "input_tokens": 1_000_000,
        "output_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    });
    let results_body = format!(
        "{}\n",
        serde_json::json!({
            "custom_id": id.to_string(),
            "result": {
                "type": "succeeded",
                "message": {
                    "id": "msg_priced",
                    "model": "claude-haiku-4-5",
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": usage,
                },
            },
        }),
    );
    let http = MockHttp::queued(vec![
        (200, submit_body),
        (200, status_body),
        (200, results_body),
    ])
    .await;
    let coord = coordinator(
        &fx,
        &http.endpoint,
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![triage_handler() as Arc<dyn PassHandler>],
    );
    let batch_id = coord.maybe_submit("triage").await.unwrap().unwrap();
    coord.poll(&batch_id).await.unwrap();

    let calls = audit::query_calls(&fx.db, &audit::AuditFilter::default(), 10, None)
        .await
        .unwrap();
    assert_eq!(calls.len(), 1);
    // claude-haiku-4-5 is $1.00/MTok input; 1,000,000 input tokens is $1.00
    // live, $0.50 batched.
    let live_price = audit::estimate_cost_usd("claude-haiku-4-5", calls[0].usage);
    assert!(
        (live_price - 1.0).abs() < 1e-9,
        "sanity: live price should be $1.00, was {live_price}"
    );
    assert!(
        (calls[0].cost_usd - 0.5).abs() < 1e-9,
        "a batch result must be ledgered at half the live price; got {}",
        calls[0].cost_usd
    );
}

#[tokio::test]
async fn release_returns_an_uncharged_lease_to_pending() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let lease = fx.queue.lease("w", 1, None).await.unwrap().remove(0);
    assert_eq!(lease.attempts, 1, "lease charges an attempt up front");

    let held = fx.queue.release(&lease).await.unwrap();
    assert!(held);

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.ready, 1);

    // The attempt `lease` charged must have been given back — re-leasing
    // should show attempts back at 1, not 2.
    let relea = fx.queue.lease("w2", 1, None).await.unwrap().remove(0);
    assert_eq!(
        relea.attempts, 1,
        "release must undo the attempt its own lease charged"
    );
}

#[tokio::test]
async fn dispatch_pending_releases_rather_than_charges_an_attempt_on_cancellation() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let (provider, handle) = MockProvider::new(vec![MockReply::Ok("x".to_owned())]);
    let pool = build_pool(&fx, provider, high_rpm_limits(), vec![triage_handler()]);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let summary = pool.dispatch_pending(10, &cancel).await.unwrap();
    assert_eq!(summary.completed, 0);
    assert_eq!(
        handle.call_count(),
        0,
        "a cancelled dispatch must never reach the provider"
    );

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(
        stats.ready, 1,
        "the job must return to pending, not sit leased or backing off"
    );
    // Re-lease and confirm the cancelled attempt was not charged.
    let lease = fx.queue.lease("w", 1, None).await.unwrap().remove(0);
    assert_eq!(
        lease.attempts, 1,
        "a cancelled attempt must not count against max_attempts"
    );
}

#[tokio::test]
async fn terminate_refuses_a_lease_this_worker_does_not_hold() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let stale = fx.queue.lease("worker-a", 1, None).await.unwrap().remove(0);
    fx.expire_lease(stale.job_id).await;
    fx.queue.reap_expired().await.unwrap();
    let _new_owner = fx.queue.lease("worker-b", 1, None).await.unwrap();

    let held = fx.queue.terminate(&stale, "stale terminate").await.unwrap();
    assert!(
        !held,
        "a stale lease holder must not be able to terminate a job it no longer owns"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.leased, 1, "the new owner's lease must be untouched");
}

#[tokio::test]
async fn poll_survives_a_failed_results_fetch_and_succeeds_on_a_later_call() {
    let fx = Fixture::open().await;
    let id = fx.message("hello, nothing sensitive here").await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let submit_body =
        serde_json::json!({"id": "batch_flaky", "processing_status": "in_progress"}).to_string();
    let status_body = serde_json::json!({
        "id": "batch_flaky",
        "processing_status": "ended",
        "request_counts": {"processing": 0, "succeeded": 1, "errored": 0, "canceled": 0, "expired": 0},
    })
    .to_string();
    let results_body = format!(
        "{}\n",
        serde_json::json!({
            "custom_id": id.to_string(),
            "result": {
                "type": "succeeded",
                "message": {
                    "id": "msg_1",
                    "model": "claude-haiku-4-5",
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 1, "output_tokens": 1, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
                },
            },
        }),
    );
    let http = MockHttp::queued(vec![
        (200, submit_body),
        (200, status_body.clone()),
        // The results fetch fails once...
        (500, "{\"error\":\"transient\"}".to_owned()),
        (200, status_body),
        // ...then succeeds on the next poll.
        (200, results_body),
    ])
    .await;
    let coord = coordinator(
        &fx,
        &http.endpoint,
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![triage_handler() as Arc<dyn PassHandler>],
    );

    let batch_id = coord.maybe_submit("triage").await.unwrap().unwrap();
    let first = coord.poll(&batch_id).await;
    assert!(first.is_err(), "a failed results fetch must propagate");

    // If the in-memory record had been discarded by the first (failed)
    // poll, this second call would hit the "no in-memory record"
    // FailedPrecondition instead of actually completing the job.
    let second = coord.poll(&batch_id).await.unwrap();
    let BatchPollOutcome::Completed(summary) = second else {
        unreachable!("expected the retried poll to complete, got {second:?}");
    };
    assert_eq!(summary.completed, 1);
}

#[tokio::test]
async fn poll_on_an_untracked_batch_id_fails_precondition() {
    let fx = Fixture::open().await;
    let status_body = serde_json::json!({
        "id": "batch_ghost",
        "processing_status": "ended",
        "request_counts": {"processing": 0, "succeeded": 0, "errored": 0, "canceled": 0, "expired": 0},
    })
    .to_string();
    let http = MockHttp::queued(vec![(200, status_body)]).await;
    let coord = coordinator(
        &fx,
        &http.endpoint,
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![triage_handler() as Arc<dyn PassHandler>],
    );

    let result = coord.poll("batch_ghost").await;
    match result {
        Err(e) => assert_eq!(e.reason(), ErrorReason::FailedPrecondition),
        Ok(outcome) => unreachable!("expected a FailedPrecondition error, got {outcome:?}"),
    }
}
