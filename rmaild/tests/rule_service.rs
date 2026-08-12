//! Integration test: drive `RuleService` end-to-end against an in-process
//! tonic server over a Unix domain socket, backed by a hand-rolled
//! [`MockProvider`] rather than the real Anthropic endpoint.
//!
//! Two harnesses, and the split matters:
//!
//! - [`TestServer`] builds [`rmaild::RuleApi`] directly over a mock provider,
//!   because `ClaudeProvider`'s endpoint is not configurable at the `Config`
//!   level (see `rmail_core::ai::provider`'s own docs), so no path through the
//!   real daemon-boot wiring is hermetic. This is the same "fake the one
//!   network-facing dependency, wire everything else for real" discipline
//!   `ai_service.rs` and `mail_service.rs` use.
//! - [`daemon_serving_rule_service`] boots the *real* `serve_uds_with_config`
//!   and calls `RuleService` through it, which is the only thing that proves
//!   the service is actually registered and that its scope-table rows admit a
//!   real call. A service can be perfectly implemented and never wired up;
//!   `AuditService` shipped deny-everything exactly that way.
//!
//! Covers the three behaviors this task's `verify` line names — eval, dry-run,
//! and cache reuse — plus create/list, synthesis, backtest, corrections, and
//! the `Status` codes on every input-validation path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rmail_core::ai::provider::{ChatResponse, ProviderStream, StopReason, Usage};
use rmail_core::ai::queue::RateLimiter;
use rmail_core::ai::{ChatRequest, PolicyEngine, Provider};
use rmail_core::compose::DraftStore;
use rmail_core::config::{AiLimits, AiPrivacy, RulesConfig, TagSyncMode, TagsConfig};
use rmail_core::events::{EventLog, Retention};
use rmail_core::imap::mutate::ImapMutator;
use rmail_core::mail::MailStore;
use rmail_core::rules::{ActionRunner, Classifier, ClaudeClassifier, RuleEngine, RuleSynthesizer};
use rmail_core::tags::TagStore;
use rmail_core::{repo, Config, Database, Error};
use rmail_proto::v1::rule_service_client::RuleServiceClient;
use rmail_proto::v1::{
    BacktestRuleRequest, CreateRuleRequest, EvaluateRulesRequest, ListRulesRequest,
    RecordCorrectionRequest, SynthesizeRuleRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Server};
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_path(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rmail-rulesvc-{label}-{pid}-{n}"))
}

/// A rule mixing a cheap `from` regex with a `claude_is`, archiving and
/// labelling what it matches.
const MIXED_RULE: &str = r#"
[[rules]]
name = "cold-pitch"

[rules.when]
from = "@coldmail\\.example>?$"
claude_is = "a cold sales pitch"

[rules.then]
add_labels = ["sales"]
notify = true
"#;

// ---------------------------------------------------------------------------
// Doubles
// ---------------------------------------------------------------------------

/// Succeeds on every IMAP method and records nothing — there is no live server
/// to dial in-process, and this suite's assertions are about the rules engine
/// rather than about IMAP wire bytes (which `mail_service.rs` covers).
#[derive(Debug, Default)]
struct FakeImap;

#[async_trait]
impl ImapMutator for FakeImap {
    async fn set_flags(&self, _: i64, _: &str, _: i64, _: i64, _: &[String]) -> Result<(), Error> {
        Ok(())
    }
    async fn move_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Ok(())
    }
    async fn copy_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Ok(())
    }
    async fn delete_message(&self, _: i64, _: &str, _: i64, _: i64) -> Result<(), Error> {
        Ok(())
    }
    async fn store_keyword(
        &self,
        _: i64,
        _: &str,
        _: i64,
        _: &[i64],
        _: &str,
        _: bool,
        _: bool,
    ) -> Result<(), Error> {
        Ok(())
    }
}

/// A scriptable provider. Running out of scripted replies is an error rather
/// than a default answer, so an unexpected extra call fails the test loudly
/// instead of quietly succeeding.
#[derive(Debug, Default)]
struct MockProvider {
    completions: Mutex<VecDeque<String>>,
    calls: AtomicUsize,
}

impl MockProvider {
    fn queue(&self, body: String) {
        self.completions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(body);
    }

    fn queue_verdict(&self, verdict: bool, explanation: &str) {
        self.queue(
            serde_json::json!({ "verdict": verdict, "explanation": explanation }).to_string(),
        );
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self
            .completions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match next {
            Some(text) => Ok(ChatResponse {
                id: "msg_mock".to_owned(),
                model: "mock-model".to_owned(),
                stop_reason: StopReason::EndTurn,
                text,
                usage: Usage::default(),
            }),
            None => Err(Error::unavailable(
                "mock provider: no scripted reply".to_owned(),
            )),
        }
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        Err(Error::internal("mock provider: stream is not scripted"))
    }
}

// ---------------------------------------------------------------------------
// Test server
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    account_id: i64,
    inbox_id: i64,
    provider: Arc<MockProvider>,
    next_uid: std::sync::atomic::AtomicI64,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        let socket = unique_path("sock");
        let db_path = unique_path("db");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);

        let db = Database::open(&db_path).unwrap();
        let (account_id, inbox_id) = db
            .write(|conn| {
                let account_id = repo::insert_account(
                    conn,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        username: Some("me@example.com".to_owned()),
                        ..Default::default()
                    },
                )?;
                let inbox_id = repo::insert_mailbox(
                    conn,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                repo::insert_mailbox(
                    conn,
                    &repo::NewMailbox {
                        account_id,
                        name: "Archive".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox_id))
            })
            .await
            .unwrap();

        let events = EventLog::new(db.clone(), Retention::unlimited());
        let provider = Arc::new(MockProvider::default());
        // `PolicyEngine::new` is `#[cfg(test)]` inside `rmail-core`, so an
        // integration test must go through the real `from_config` path — the
        // same one production code uses.
        let policy = Arc::new(PolicyEngine::from_config(&Config::default()).unwrap());
        let limits = AiLimits {
            requests_per_minute: 1_000_000,
            daily_token_cap: 1_000_000_000,
            daily_cost_cap_usd: 1_000.0,
            monthly_cost_cap_usd: 1_000.0,
            ..AiLimits::default()
        };
        let semaphore = Arc::new(tokio::sync::Semaphore::new(4));
        let rate_limiter = Arc::new(RateLimiter::new(1_000_000));
        let imap: Arc<dyn ImapMutator> = Arc::new(FakeImap);

        let engine = RuleEngine::new(
            db.clone(),
            RulesConfig::default().rule_limits(),
            Arc::new(ClaudeClassifier::new(
                db.clone(),
                Arc::clone(&provider) as Arc<dyn Provider>,
                Arc::clone(&policy),
                AiPrivacy::default(),
                limits.clone(),
                "mock-model",
                8,
                Arc::clone(&semaphore),
                Arc::clone(&rate_limiter),
            )) as Arc<dyn Classifier>,
            ActionRunner::new(
                db.clone(),
                MailStore::new(db.clone(), events.clone(), Arc::clone(&imap)),
                TagStore::new(
                    db.clone(),
                    Arc::clone(&imap),
                    TagsConfig {
                        default_sync_mode: TagSyncMode::Local,
                        ..TagsConfig::default()
                    },
                ),
                DraftStore::new(db.clone()),
                events.clone(),
                Vec::new(),
                Arc::new(tokio::sync::Semaphore::new(2)),
                64 * 1024,
                "Archive",
            ),
            Arc::clone(&policy),
            500,
        );
        let stopping = CancellationToken::new();
        let api = rmaild::RuleApi::new(
            engine.clone(),
            RuleSynthesizer::new(
                engine,
                Arc::clone(&provider) as Arc<dyn Provider>,
                policy,
                AiPrivacy::default(),
                limits,
                "mock-model",
                semaphore,
                rate_limiter,
            ),
            30,
            stopping.clone(),
        );

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let incoming = UnixListenerStream::new(listener);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(rmail_proto::v1::rule_service_server::RuleServiceServer::new(api))
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = shutdown_rx.await;
                    stopping.cancel();
                })
                .await;
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
            account_id,
            inbox_id,
            provider,
            next_uid: std::sync::atomic::AtomicI64::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> RuleServiceClient<Channel> {
        RuleServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    async fn message(&self, from_addr: &str, subject: &str, body: &str) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let account_id = self.account_id;
        let mailbox_id = self.inbox_id;
        let from_addr = from_addr.to_owned();
        let subject = subject.to_owned();
        let body = body.to_owned();
        self.db
            .write(move |conn| {
                repo::insert_message(
                    conn,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        message_id: Some(format!("<m{uid}@example.com>")),
                        from_addr: Some(from_addr),
                        from_name: Some("Sender".to_owned()),
                        subject: Some(subject),
                        body_text: Some(body),
                        date: Some(chrono::Utc::now().timestamp()),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn tag_count(&self) -> i64 {
        self.db
            .read(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM message_tags WHERE state = 'applied'",
                    [],
                    |r| r.get(0),
                )
            })
            .await
            .unwrap()
    }

    async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

// ---------------------------------------------------------------------------
// CreateRule / ListRules
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_then_list_returns_the_document_verbatim() {
    let server = TestServer::start().await;
    let created = server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id,
            toml: MIXED_RULE.to_owned(),
        })
        .await
        .unwrap()
        .into_inner()
        .rule
        .expect("a rule");
    assert_eq!(created.name, "cold-pitch");
    assert!(created.enabled);

    let listed = server
        .client()
        .await
        .list_rules(ListRulesRequest {
            account_id: server.account_id,
        })
        .await
        .unwrap()
        .into_inner()
        .rules;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].toml, MIXED_RULE);
    server.shutdown().await;
}

#[tokio::test]
async fn create_rejects_a_malformed_or_unsafe_document_with_invalid_argument() {
    let server = TestServer::start().await;
    for (label, toml) in [
        ("not toml", "this is not toml at all ["),
        (
            "no predicates",
            "[[rules]]\nname = \"x\"\n[rules.when]\n[rules.then]\nnotify = true\n",
        ),
        (
            "no actions",
            "[[rules]]\nname = \"x\"\n[rules.when]\nfrom = \"a\"\n[rules.then]\n",
        ),
        (
            "regex bomb",
            "[[rules]]\nname = \"x\"\n[rules.when]\nsubject = \"(a{1000}){1000}\"\n\
             [rules.then]\nnotify = true\n",
        ),
        (
            "two rules in one document",
            "[[rules]]\nname = \"a\"\n[rules.when]\nfrom = \"a\"\n[rules.then]\nnotify = true\n\
             [[rules]]\nname = \"b\"\n[rules.when]\nfrom = \"b\"\n[rules.then]\nnotify = true\n",
        ),
    ] {
        let status = server
            .client()
            .await
            .create_rule(CreateRuleRequest {
                account_id: server.account_id,
                toml: toml.to_owned(),
            })
            .await
            .expect_err(label);
        assert_eq!(status.code(), Code::InvalidArgument, "{label}");
    }
    server.shutdown().await;
}

#[tokio::test]
async fn create_reports_a_duplicate_name_and_a_missing_account() {
    let server = TestServer::start().await;
    server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id,
            toml: MIXED_RULE.to_owned(),
        })
        .await
        .unwrap();

    let status = server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id,
            toml: MIXED_RULE.to_owned(),
        })
        .await
        .expect_err("duplicate");
    assert_eq!(status.code(), Code::AlreadyExists);

    let status = server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id + 999,
            toml: MIXED_RULE.to_owned(),
        })
        .await
        .expect_err("no account");
    assert_eq!(status.code(), Code::NotFound);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// EvaluateRules: eval, action firing, at-most-once, and cache reuse
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evaluate_fires_actions_and_reuses_the_classification_cache() {
    // The task's `verify` line in one test: an evaluation that fires, then a
    // second evaluation of the same message that costs no provider call at
    // all — once because the actions were already claimed, and once because
    // the `claude_is` verdict is cached by message-id + prompt-hash.
    let server = TestServer::start().await;
    server.provider.queue_verdict(true, "it is a pitch");
    server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id,
            toml: MIXED_RULE.to_owned(),
        })
        .await
        .unwrap();
    let pitch = server
        .message("bot@coldmail.example", "quick question", "buy now")
        .await;

    let first = server
        .client()
        .await
        .evaluate_rules(EvaluateRulesRequest {
            account_id: server.account_id,
            message_ids: vec![pitch],
            rule_names: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let stats = first.stats.expect("stats");
    assert_eq!(stats.matches, 1);
    assert_eq!(stats.model_calls, 1);
    assert_eq!(stats.cache_hits, 0);
    assert!(
        stats.actions_applied >= 2,
        "add_labels and notify both fired"
    );
    assert_eq!(stats.actions_failed, 0);
    assert_eq!(
        first.messages[0].rules[0].explanation, "it is a pitch",
        "the Claude explanation for the claude_is decision is reported"
    );
    assert_eq!(server.tag_count().await, 1);

    let second = server
        .client()
        .await
        .evaluate_rules(EvaluateRulesRequest {
            account_id: server.account_id,
            message_ids: vec![pitch],
            rule_names: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let stats = second.stats.expect("stats");
    assert_eq!(stats.matches, 1);
    assert_eq!(stats.model_calls, 0, "the verdict must come from the cache");
    assert_eq!(stats.cache_hits, 1);
    assert!(second.messages[0].rules[0].already_fired);
    assert!(second.messages[0].rules[0].actions.is_empty());
    assert_eq!(
        server.provider.calls(),
        1,
        "exactly one provider call across both evaluations"
    );
    assert_eq!(server.tag_count().await, 1, "no duplicate tag application");
    server.shutdown().await;
}

#[tokio::test]
async fn evaluate_reports_a_predicate_trace_including_what_was_never_asked() {
    let server = TestServer::start().await;
    server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id,
            toml: MIXED_RULE.to_owned(),
        })
        .await
        .unwrap();
    let friend = server
        .message("friend@example.com", "lunch?", "are you free")
        .await;

    let report = server
        .client()
        .await
        .evaluate_rules(EvaluateRulesRequest {
            account_id: server.account_id,
            message_ids: vec![friend],
            rule_names: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();

    let rule = &report.messages[0].rules[0];
    assert!(!rule.matched);
    let claude = rule
        .predicates
        .iter()
        .find(|p| p.predicate == "claude_is")
        .expect("claude_is reported");
    assert!(!claude.evaluated, "the model must not have been asked");
    assert!(!claude.detail.is_empty(), "and it must say why");
    assert_eq!(
        server.provider.calls(),
        0,
        "a failed cheap predicate must cost nothing"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn evaluate_rejects_an_empty_or_over_long_message_list() {
    let server = TestServer::start().await;
    for ids in [Vec::new(), (0..600).collect::<Vec<i64>>()] {
        let status = server
            .client()
            .await
            .evaluate_rules(EvaluateRulesRequest {
                account_id: server.account_id,
                message_ids: ids,
                rule_names: Vec::new(),
            })
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), Code::InvalidArgument);
    }
    server.shutdown().await;
}

#[tokio::test]
async fn evaluate_rejects_an_over_long_rule_name_list() {
    // One blocking-pool lookup per name; an unbounded list is an unbounded
    // number of round trips issued from one request.
    let server = TestServer::start().await;
    let message = server.message("a@example.com", "s", "b").await;
    let status = server
        .client()
        .await
        .evaluate_rules(EvaluateRulesRequest {
            account_id: server.account_id,
            message_ids: vec![message],
            rule_names: (0..200).map(|n| format!("r{n}")).collect(),
        })
        .await
        .expect_err("must be refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.shutdown().await;
}

#[tokio::test]
async fn evaluate_naming_a_disabled_rule_is_failed_precondition() {
    // Firing it would also burn its at-most-once claim, so enabling the rule
    // later would never re-fire it for those messages.
    let server = TestServer::start().await;
    server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id,
            toml: "[[rules]]\nname = \"off\"\nenabled = false\n[rules.when]\n\
                   from = \"coldmail\"\n[rules.then]\nnotify = true\n"
                .to_owned(),
        })
        .await
        .unwrap();
    let pitch = server.message("bot@coldmail.example", "hi", "buy").await;

    let status = server
        .client()
        .await
        .evaluate_rules(EvaluateRulesRequest {
            account_id: server.account_id,
            message_ids: vec![pitch],
            rule_names: vec!["off".to_owned()],
        })
        .await
        .expect_err("must be refused");
    assert_eq!(status.code(), Code::FailedPrecondition);

    // ...but backtesting it is exactly how you find out what it would do.
    let report = server
        .client()
        .await
        .backtest_rule(BacktestRuleRequest {
            account_id: server.account_id,
            rule_name: "off".to_owned(),
            rule_toml: String::new(),
            days: 30,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(report.messages[0].rules[0].matched);
    server.shutdown().await;
}

#[tokio::test]
async fn evaluate_naming_an_unknown_rule_is_not_found() {
    let server = TestServer::start().await;
    let message = server.message("a@example.com", "s", "b").await;
    let status = server
        .client()
        .await
        .evaluate_rules(EvaluateRulesRequest {
            account_id: server.account_id,
            message_ids: vec![message],
            rule_names: vec!["nope".to_owned()],
        })
        .await
        .expect_err("must be refused");
    assert_eq!(status.code(), Code::NotFound);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// BacktestRule: dry run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn backtest_reports_per_message_outcomes_and_fires_nothing() {
    let server = TestServer::start().await;
    server.provider.queue_verdict(true, "clearly a pitch");
    let pitch = server
        .message("bot@coldmail.example", "quick question", "buy now")
        .await;
    server.message("friend@example.com", "lunch", "free?").await;

    let report = server
        .client()
        .await
        .backtest_rule(BacktestRuleRequest {
            account_id: server.account_id,
            rule_name: String::new(),
            rule_toml: MIXED_RULE.to_owned(),
            days: 30,
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(report.window_days, 30);
    assert_eq!(report.messages.len(), 2);
    let stats = report.stats.expect("stats");
    assert_eq!(stats.matches, 1);
    assert_eq!(stats.model_calls, 1);
    assert_eq!(stats.actions_applied, 0, "a dry run applies nothing");
    let hit = report
        .messages
        .iter()
        .find(|m| m.message_id == pitch)
        .expect("the pitch is reported");
    assert_eq!(hit.rules[0].explanation, "clearly a pitch");
    assert_eq!(hit.rfc_message_id, "<m1@example.com>");
    assert!(
        hit.rules[0].actions.iter().all(|a| !a.applied),
        "no action may report itself applied"
    );
    assert!(hit.rules[0]
        .actions
        .iter()
        .any(|a| a.detail.starts_with("would ")));
    assert_eq!(server.tag_count().await, 0, "nothing was tagged");
    server.shutdown().await;
}

#[tokio::test]
async fn backtest_requires_exactly_one_of_rule_name_and_rule_toml() {
    let server = TestServer::start().await;
    for (name, toml) in [
        (String::new(), String::new()),
        ("cold-pitch".to_owned(), MIXED_RULE.to_owned()),
    ] {
        let status = server
            .client()
            .await
            .backtest_rule(BacktestRuleRequest {
                account_id: server.account_id,
                rule_name: name,
                rule_toml: toml,
                days: 7,
            })
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), Code::InvalidArgument);
    }
    server.shutdown().await;
}

#[tokio::test]
async fn backtest_of_a_stored_rule_never_claims_it_so_a_later_evaluate_still_fires() {
    let server = TestServer::start().await;
    server.provider.queue_verdict(true, "a pitch");
    server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id,
            toml: MIXED_RULE.to_owned(),
        })
        .await
        .unwrap();
    let pitch = server
        .message("bot@coldmail.example", "hi", "buy now")
        .await;

    server
        .client()
        .await
        .backtest_rule(BacktestRuleRequest {
            account_id: server.account_id,
            rule_name: "cold-pitch".to_owned(),
            rule_toml: String::new(),
            days: 30,
        })
        .await
        .unwrap();
    assert_eq!(server.tag_count().await, 0);

    let report = server
        .client()
        .await
        .evaluate_rules(EvaluateRulesRequest {
            account_id: server.account_id,
            message_ids: vec![pitch],
            rule_names: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!report.messages[0].rules[0].already_fired);
    assert_eq!(server.tag_count().await, 1);
    assert_eq!(
        server.provider.calls(),
        1,
        "the backtest's cached verdict is reused by the real evaluation"
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// SynthesizeRule
// ---------------------------------------------------------------------------

fn proposal(name: &str, from: &str, claude_is: &str) -> String {
    serde_json::json!({
        "name": name,
        "match": "all",
        "from": from,
        "subject": "",
        "body": "",
        "headers": [],
        "has_flags": [],
        "lacks_flags": [],
        "min_bytes": 0,
        "max_bytes": 0,
        "claude_is": claude_is,
        "move_to": "",
        "archive": true,
        "add_labels": [],
        "add_flags": [],
        "notify": false,
        "run_hook": "",
        "draft_reply": "",
        "notes": "Archives cold pitches.",
    })
    .to_string()
}

#[tokio::test]
async fn synthesize_returns_a_creatable_rule_and_a_dry_run_over_the_window() {
    let server = TestServer::start().await;
    server
        .provider
        .queue(proposal("cold-pitch", "@coldmail\\.example", ""));
    server
        .message("bot@coldmail.example", "quick question", "buy now")
        .await;
    server.message("friend@example.com", "lunch", "free?").await;

    let response = server
        .client()
        .await
        .synthesize_rule(SynthesizeRuleRequest {
            account_id: server.account_id,
            instruction: "archive cold sales pitches".to_owned(),
            days: 30,
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.name, "cold-pitch");
    assert!(!response.uses_claude_is, "a deterministic-only proposal");
    assert_eq!(response.notes, "Archives cold pitches.");
    assert_eq!(response.window_days, 30);
    assert_eq!(response.dry_run.len(), 2);
    assert_eq!(response.stats.expect("stats").matches, 1);

    // The returned document is what CreateRule accepts verbatim — the whole
    // point of returning TOML rather than a structured rule.
    let created = server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id,
            toml: response.toml,
        })
        .await
        .unwrap()
        .into_inner()
        .rule
        .expect("a rule");
    assert_eq!(created.name, "cold-pitch");
    server.shutdown().await;
}

#[tokio::test]
async fn synthesize_drops_a_redundant_claude_is_and_says_so() {
    let server = TestServer::start().await;
    server.provider.queue(proposal(
        "cold-pitch",
        "@coldmail\\.example",
        "a cold sales pitch",
    ));
    // The full pass's one classification agrees with the cheap predicate,
    // which is the redundancy under test.
    server.provider.queue_verdict(true, "it is a pitch");
    server
        .message("bot@coldmail.example", "quick question", "buy now")
        .await;

    let response = server
        .client()
        .await
        .synthesize_rule(SynthesizeRuleRequest {
            account_id: server.account_id,
            instruction: "archive cold sales pitches".to_owned(),
            days: 30,
        })
        .await
        .unwrap()
        .into_inner();

    assert!(!response.uses_claude_is);
    assert!(
        response.claude_is_dropped.contains("changed no outcome"),
        "got {:?}",
        response.claude_is_dropped
    );
    assert!(!response.toml.contains("claude_is"));
    server.shutdown().await;
}

#[tokio::test]
async fn synthesize_rejects_an_empty_instruction() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .synthesize_rule(SynthesizeRuleRequest {
            account_id: server.account_id,
            instruction: "  ".to_owned(),
            days: 0,
        })
        .await
        .expect_err("must be refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(server.provider.calls(), 0);
    server.shutdown().await;
}

#[tokio::test]
async fn a_provider_failure_surfaces_as_unavailable_rather_than_a_wrong_answer() {
    let server = TestServer::start().await;
    // Nothing queued: the mock refuses with `Unavailable`.
    let status = server
        .client()
        .await
        .synthesize_rule(SynthesizeRuleRequest {
            account_id: server.account_id,
            instruction: "archive newsletters".to_owned(),
            days: 7,
        })
        .await
        .expect_err("must fail");
    assert_eq!(status.code(), Code::Unavailable);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// RecordCorrection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_correction_changes_the_verdict_without_a_second_provider_call() {
    let server = TestServer::start().await;
    server.provider.queue_verdict(true, "it is a pitch");
    server
        .client()
        .await
        .create_rule(CreateRuleRequest {
            account_id: server.account_id,
            toml: MIXED_RULE.to_owned(),
        })
        .await
        .unwrap();
    let pitch = server
        .message("bot@coldmail.example", "quick question", "buy now")
        .await;

    let before = server
        .client()
        .await
        .backtest_rule(BacktestRuleRequest {
            account_id: server.account_id,
            rule_name: "cold-pitch".to_owned(),
            rule_toml: String::new(),
            days: 30,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(before.messages[0].rules[0].matched);

    let recorded = server
        .client()
        .await
        .record_correction(RecordCorrectionRequest {
            account_id: server.account_id,
            message_id: pitch,
            prompt: "a cold sales pitch".to_owned(),
            expected: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(recorded.example_count, 1);

    let after = server
        .client()
        .await
        .backtest_rule(BacktestRuleRequest {
            account_id: server.account_id,
            rule_name: "cold-pitch".to_owned(),
            rule_toml: String::new(),
            days: 30,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        !after.messages[0].rules[0].matched,
        "the user's correction must win"
    );
    assert_eq!(server.provider.calls(), 1);
    server.shutdown().await;
}

#[tokio::test]
async fn a_correction_naming_a_missing_message_or_an_empty_prompt_is_refused() {
    let server = TestServer::start().await;
    let message = server.message("a@example.com", "s", "b").await;

    let status = server
        .client()
        .await
        .record_correction(RecordCorrectionRequest {
            account_id: server.account_id,
            message_id: message,
            prompt: "   ".to_owned(),
            expected: true,
        })
        .await
        .expect_err("empty prompt");
    assert_eq!(status.code(), Code::InvalidArgument);

    let status = server
        .client()
        .await
        .record_correction(RecordCorrectionRequest {
            account_id: server.account_id,
            message_id: message + 9_999,
            prompt: "a cold sales pitch".to_owned(),
            expected: true,
        })
        .await
        .expect_err("missing message");
    assert_eq!(status.code(), Code::NotFound);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// The real daemon boot
// ---------------------------------------------------------------------------

/// `RuleService` is genuinely registered on the daemon, reachable through the
/// real `serve_uds_with_config` path, and admitted by the capability-scope
/// table. A service can be perfectly implemented and never wired up — see this
/// file's module docs.
#[tokio::test]
async fn daemon_serving_rule_service() {
    let socket = unique_path("daemon-sock");
    let db_path = unique_path("daemon-db");
    let db = Database::open(&db_path).unwrap();
    let account_id = db
        .write(|conn| {
            repo::insert_account(
                conn,
                &repo::NewAccount {
                    name: "Personal".to_owned(),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();

    let mut config = Config::default();
    config.index.semantic.enabled = false;
    config.ai.enabled = false;
    // The background evaluator would otherwise tick against the event log for
    // the whole test; this suite is about the served surface.
    config.rules.enabled = false;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_socket = socket.clone();
    let handle = tokio::spawn(async move {
        rmaild::serve_uds_with_config(&server_socket, db, config, async move {
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
    assert!(ready, "daemon never became ready");

    let channel = rmail_core::connect_uds(&socket).await.unwrap();
    let mut client = RuleServiceClient::new(channel);
    client
        .create_rule(CreateRuleRequest {
            account_id,
            toml: "[[rules]]\nname = \"deterministic\"\n[rules.when]\nfrom = \"coldmail\"\n\
                   [rules.then]\nnotify = true\n"
                .to_owned(),
        })
        .await
        .expect("CreateRule must be served and admitted by the scope table");
    let rules = client
        .list_rules(ListRulesRequest { account_id })
        .await
        .expect("ListRules must be served")
        .into_inner()
        .rules;
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "deterministic");

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
    let _ = std::fs::remove_file(&socket);
}
