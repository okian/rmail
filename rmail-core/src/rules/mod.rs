//! The rules engine (task 66; prd.md #45 "AI Classification Rules Engine",
//! #46 "Natural-Language Rule Synthesis", #50 "Rule Backtest & Explain").
//!
//! A rule is a TOML document pairing deterministic predicates
//! (`from`/`subject`/`body`/`header` regexes, flag membership, size bounds)
//! with an optional `claude_is` natural-language predicate, and an action
//! block (`move_to`/`archive`/`add_labels`/`add_flags`/`notify`/`run_hook`/
//! `draft_reply`). [`RuleEngine`] is the whole surface: create, list,
//! evaluate, dry-run, backtest, synthesize, and correct.
//!
//! ```text
//! new mail ──▶ RuleEvaluator ──▶ RuleEngine::evaluate_message
//!                                    │
//!                       eval (cheap predicates first)
//!                                    │
//!                     claude_is? ──▶ cache ──▶ Classifier ──▶ provider
//!                                    │
//!                      claim (rule, message) ──▶ ActionRunner
//! ```
//!
//! # Cost is the axis this module is organized around
//!
//! Everything else here follows from one fact: a `claude_is` predicate costs
//! money, on every new message, unattended. Four separate mechanisms hold
//! that down, and each is somewhere different:
//!
//! 1. **Cheap predicates decide first.** [`eval::evaluate`] resolves every
//!    deterministic predicate before consulting the model, and skips the
//!    model entirely when they have already settled the rule.
//! 2. **Verdicts are cached** by `message-id + prompt-hash`
//!    ([`classify::prompt_hash`]), so a message re-evaluated by a second rule
//!    with the same predicate, a backtest, and the live evaluator all share
//!    one answer.
//! 3. **Synthesis prefers the deterministic form** and, when the model
//!    proposes both, [`synth`] *empirically checks* whether the `claude_is`
//!    changed any outcome over the dry-run window and drops it when it did
//!    not — see that module's docs.
//! 4. **The provider call goes through the AI subsystem's own gates**:
//!    policy, cost gate, per-account budget, shared semaphore, shared RPM
//!    limiter. A rules engine is precisely the unattended workload those
//!    exist for.
//!
//! # Why evaluation is claim-then-act
//!
//! A rule's actions are side effects with no natural idempotency key. See
//! [`actions`]'s module docs for the full argument; the short version is that
//! `rule_actions_fired` is claimed *before* the actions run, so the crash
//! window costs one message's actions rather than an unbounded number of
//! duplicated drafts and hook runs. It is also what makes the background
//! evaluator and a concurrent `EvaluateRules` RPC safe to run at the same
//! moment without an in-process lock.
//!
//! # Rules are per account, and evaluation never crosses one
//!
//! Every entry point takes an `account_id` and every query is scoped by it.
//! A rule cannot match, classify, or act on a message in another account —
//! which matters because `ai.policy` eligibility is resolved per account and
//! folder, and a rule that could reach across accounts would be a way around
//! it.

pub mod actions;
pub mod classify;
pub mod eval;
pub mod facts;
pub mod gate;
pub mod model;
pub mod repo;
pub mod synth;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ai::policy::{PolicyEngine, PolicyTarget};
use crate::error::{Error, ErrorReason};
use crate::events::{EventKind, EventLog};
use crate::storage::Database;

pub use actions::{ActionOutcome, ActionRunner};
pub use classify::{prompt_hash, ClaudeClassifier, Example};
pub use eval::{Classification, Classifier, Evaluation, Outcome};
pub use facts::MessageFacts;
pub use model::{
    parse_document, parse_single, to_document, Actions, Compiled, MatchMode, Predicates,
    RuleDocument, RuleLimits, RuleSpec,
};
pub use repo::StoredRule;
pub use synth::{RuleSynthesizer, Synthesis};

/// How many durable-log events one [`RuleEvaluator::tick`] page reads —
/// the same value and reasoning as `hooks::DRAIN_PAGE` and
/// `smart_folder::DRAIN_PAGE`.
const DRAIN_PAGE: i64 = 500;

/// Default interval between evaluator ticks. Matches
/// [`crate::hooks::DEFAULT_TICK_INTERVAL`]: this is the upper bound on how
/// long after a message arrives its rules fire.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Floor on the configured tick interval, so a `tick_interval = "0s"` typo
/// cannot turn the evaluator into a busy loop against the event log.
pub const MIN_TICK_INTERVAL: Duration = Duration::from_millis(10);

/// How many messages one tick will evaluate before deferring the rest to the
/// next one. An initial sync landing thousands of messages must not become
/// one unbounded evaluation pass holding thousands of snapshots in memory —
/// the same bound `hooks::DEFAULT_MAX_BATCH` places on matched hooks.
pub const DEFAULT_MAX_BATCH: usize = 200;

/// What one rule did for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleReport {
    /// The rule's name.
    pub rule: String,
    /// Whether it matched.
    pub matched: bool,
    /// The per-predicate trace, in evaluation order.
    pub outcomes: Vec<Outcome>,
    /// What its actions did (or, on a dry run, would do). Empty when the rule
    /// did not match.
    pub actions: Vec<ActionOutcome>,
    /// Whether this rule had already acted on this message. On a real run the
    /// actions are skipped; on a dry run they are still described, flagged by
    /// this.
    pub already_fired: bool,
    /// Whether a `claude_is` was answered by a fresh provider call.
    pub model_called: bool,
    /// Whether a `claude_is` was answered without one (cache hit, or a user
    /// correction for this exact message).
    pub cache_hit: bool,
    /// The model's explanation for the `claude_is` decision, when it was
    /// consulted — prd.md #50's "one-line Claude explanation per `claude_is`
    /// decision."
    pub explanation: Option<String>,
}

/// What every rule did for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageReport {
    /// The local message id.
    pub message_id: i64,
    /// The RFC822 `Message-ID`, for correlating with another client.
    pub rfc_message_id: Option<String>,
    /// The subject, for a readable report.
    pub subject: String,
    /// The sender, for a readable report.
    pub from: String,
    /// One entry per rule considered.
    pub rules: Vec<RuleReport>,
    /// Set when this message could not be evaluated at all (it vanished, or
    /// a `claude_is` could not be answered). One message failing never fails
    /// the whole run — see [`RuleEngine::evaluate`].
    pub error: Option<String>,
}

/// The aggregate result of one evaluation, dry run, or backtest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvaluationReport {
    /// Whether this run was a dry run (nothing mutated).
    pub dry_run: bool,
    /// Per-message detail, in the order the messages were considered.
    pub messages: Vec<MessageReport>,
    /// How many (message, rule) pairs matched.
    pub matches: usize,
    /// How many provider calls were made.
    pub model_calls: usize,
    /// How many `claude_is` answers came from the cache or a correction.
    pub cache_hits: usize,
    /// How many actions were applied.
    pub actions_applied: usize,
    /// How many actions were attempted and failed.
    pub actions_failed: usize,
    /// How many messages could not be evaluated.
    pub errors: usize,
}

impl EvaluationReport {
    fn absorb(&mut self, report: MessageReport) {
        if report.error.is_some() {
            self.errors += 1;
        }
        for rule in &report.rules {
            if rule.matched {
                self.matches += 1;
            }
            if rule.model_called {
                self.model_calls += 1;
            }
            if rule.cache_hit {
                self.cache_hits += 1;
            }
            if !self.dry_run {
                for action in &rule.actions {
                    if action.applied {
                        self.actions_applied += 1;
                    } else {
                        self.actions_failed += 1;
                    }
                }
            }
        }
        self.messages.push(report);
    }
}

/// Which rules an evaluation considers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSelector {
    /// Every enabled rule in the account, alphabetically. What the background
    /// evaluator uses.
    AllEnabled,
    /// Named rules, whether enabled or not — an operator testing a rule they
    /// have not turned on yet.
    Named(Vec<String>),
    /// An unsaved rule document, for `BacktestRule`'s "what would this rule
    /// have done" before it is ever created. Never claims or fires anything:
    /// an ad-hoc rule has no row to claim against.
    Ad(Box<RuleSpec>),
}

/// The rules engine.
///
/// Cheap to clone — every field is a handle. One instance serves the
/// background [`RuleEvaluator`] and the daemon's `RuleService` handlers, which
/// is what makes the `rule_actions_fired` claim meaningful between them.
#[derive(Debug, Clone)]
pub struct RuleEngine {
    db: Database,
    limits: RuleLimits,
    classifier: Arc<dyn Classifier>,
    actions: ActionRunner,
    /// Resolves `ai.policy` for [`RuleEngine::record_correction`].
    ///
    /// The classifier resolves policy for the message it is about to send
    /// (see [`gate::admit`]), which covers the evaluation path — but a
    /// *correction* freezes a rendered copy of its message into
    /// `rule_examples`, and every later classification of that predicate
    /// replays it to the provider as a prior turn. Without this check, a
    /// correction recorded on a message in a `forbidden`/`local_only` folder
    /// would smuggle that folder's content out on the next classification of
    /// a message in an allowed one. Redaction is not policy exclusion: the
    /// guard tokenizes PII, it does not remove the prose.
    policy: Arc<PolicyEngine>,
    /// Cap on how many messages one backtest/dry-run window materializes.
    max_window: usize,
}

impl RuleEngine {
    /// Build an engine.
    #[must_use]
    pub fn new(
        db: Database,
        limits: RuleLimits,
        classifier: Arc<dyn Classifier>,
        actions: ActionRunner,
        policy: Arc<PolicyEngine>,
        max_window: usize,
    ) -> Self {
        Self {
            db,
            limits,
            classifier,
            actions,
            policy,
            // A zero here would make every backtest silently empty; one is
            // the smallest window that still answers a question.
            max_window: max_window.max(1),
        }
    }

    /// The pattern bounds this engine validates and compiles under.
    #[must_use]
    pub fn limits(&self) -> &RuleLimits {
        &self.limits
    }

    /// The database this engine reads and writes. Exposed for
    /// [`synth::RuleSynthesizer`], which audits its own provider call against
    /// the same ledger rather than being handed a second `Database` handle
    /// that could, through a wiring mistake, point somewhere else.
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Parse, validate, and persist one rule.
    ///
    /// `document` must contain exactly one `[[rules]]` entry — see
    /// [`model::parse_single`]. It is stored verbatim, so listing it returns
    /// what the author wrote.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for a malformed or unsafe document,
    /// [`Error::AlreadyExists`] for a duplicate name, [`Error::NotFound`] if
    /// the account does not exist.
    #[tracing::instrument(skip(self, document), fields(account_id, rule), err)]
    pub async fn create(&self, account_id: i64, document: &str) -> Result<StoredRule, Error> {
        let spec = model::parse_single(document, &self.limits)?;
        tracing::Span::current().record("rule", spec.name.as_str());
        repo::insert_rule(
            &self.db,
            account_id,
            spec.name.trim(),
            document,
            spec.enabled,
        )
        .await
    }

    /// One account's rules.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn list(&self, account_id: i64) -> Result<Vec<StoredRule>, Error> {
        repo::list_rules(&self.db, account_id).await
    }

    /// Resolve `selector` into compiled rules paired with their row id
    /// (`None` for an ad-hoc rule, which has no row and therefore never
    /// claims).
    ///
    /// A stored rule that no longer compiles under the *current* limits is
    /// reported as an error rather than skipped: it was valid when it was
    /// created, so a rule that silently stops running because an operator
    /// tightened `rules.max_pattern_len` is exactly the kind of quiet
    /// automation failure this engine must not have.
    async fn resolve(
        &self,
        account_id: i64,
        selector: &RuleSelector,
        dry_run: bool,
        cancel: &CancellationToken,
    ) -> Result<Vec<(Option<i64>, Compiled)>, Error> {
        let stored = match selector {
            RuleSelector::AllEnabled => repo::list_rules(&self.db, account_id)
                .await?
                .into_iter()
                .filter(|rule| rule.enabled)
                .collect(),
            RuleSelector::Named(names) => {
                let mut out = Vec::with_capacity(names.len());
                for name in names {
                    // Checked inside the loop, not only around it: one
                    // `get_rule` per name is a blocking-pool round trip, and a
                    // caller naming thousands of them would otherwise be
                    // uninterruptible by shutdown.
                    if cancel.is_cancelled() {
                        return Err(Error::unavailable(
                            "rule evaluation was cancelled before it finished".to_owned(),
                        ));
                    }
                    out.push(repo::get_rule(&self.db, account_id, name).await?);
                }
                out
            }
            RuleSelector::Ad(spec) => {
                // An ad-hoc rule has no row to claim against, so it can never
                // fire an action — asking it to is a caller bug, not something
                // to silently downgrade to a description that the report's
                // counters would then tally as failed actions.
                if !dry_run {
                    return Err(Error::invalid_argument(
                        "an unsaved rule cannot fire actions; create it first, or backtest it",
                    ));
                }
                return Ok(vec![(None, spec.compile(&self.limits)?)]);
            }
        };

        let mut out = Vec::with_capacity(stored.len());
        for rule in stored {
            // A disabled rule is evaluable — that is how an operator validates
            // one before turning it on — but only as a dry run. Firing it
            // would also burn the `rule_actions_fired` claim, so enabling the
            // rule later would never re-fire it for those messages.
            if !rule.enabled && !dry_run {
                return Err(Error::failed_precondition(format!(
                    "rule {:?} is disabled; enable it, or backtest it to see what it would do",
                    rule.name
                )));
            }
            // Parsed without the validating wrapper and compiled once:
            // `compile` validates as part of building, so going through
            // `parse_single` here would compile every pattern a second time,
            // on a Tokio worker, for every rule on every tick.
            let spec = model::parse_document_unvalidated(&rule.toml)
                .and_then(|mut rules| {
                    rules.pop().ok_or_else(|| {
                        Error::invalid_argument("stored rule document contains no [[rules]]")
                    })
                })
                .and_then(|spec| spec.compile(&self.limits))
                .map_err(|e| {
                    Error::failed_precondition(format!(
                        "stored rule {:?} no longer validates: {e}",
                        rule.name
                    ))
                })?;
            out.push((Some(rule.id), spec));
        }
        Ok(out)
    }

    /// Evaluate `selector`'s rules against `message_ids`.
    ///
    /// With `dry_run`, nothing is claimed and no action is fired — the report
    /// describes what would have happened. Without it, a matching rule claims
    /// `(rule, message)` and fires; a rule that has already acted on that
    /// message is reported with `already_fired` and does nothing.
    ///
    /// One message failing (it was deleted mid-run, its `claude_is` could not
    /// be answered) is recorded against that message and the run continues:
    /// a backtest over a month of mail must not be lost to one transient
    /// provider error.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if a named rule does not compile,
    /// [`Error::NotFound`] if a named rule does not exist,
    /// [`Error::Unavailable`] if `cancel` fired before the run finished.
    #[tracing::instrument(
        skip(self, message_ids, selector, cancel),
        fields(account_id, dry_run, messages = message_ids.len(), matches, model_calls),
        err
    )]
    pub async fn evaluate(
        &self,
        account_id: i64,
        message_ids: &[i64],
        selector: &RuleSelector,
        dry_run: bool,
        cancel: &CancellationToken,
    ) -> Result<EvaluationReport, Error> {
        let rules = self.resolve(account_id, selector, dry_run, cancel).await?;
        let need_headers = rules
            .iter()
            .any(|(_, compiled)| !compiled.spec.when.header.is_empty());

        let mut report = EvaluationReport {
            dry_run,
            ..EvaluationReport::default()
        };
        for &message_id in message_ids {
            if cancel.is_cancelled() {
                return Err(Error::unavailable(
                    "rule evaluation was cancelled before it finished".to_owned(),
                ));
            }
            report.absorb(
                self.evaluate_one(
                    account_id,
                    message_id,
                    &rules,
                    need_headers,
                    dry_run,
                    cancel,
                )
                .await,
            );
        }
        let span = tracing::Span::current();
        span.record("matches", report.matches);
        span.record("model_calls", report.model_calls);
        Ok(report)
    }

    /// Evaluate one message. Never returns `Err` — a failure becomes the
    /// message's own `error` field, which is what lets a long run continue.
    async fn evaluate_one(
        &self,
        account_id: i64,
        message_id: i64,
        rules: &[(Option<i64>, Compiled)],
        need_headers: bool,
        dry_run: bool,
        cancel: &CancellationToken,
    ) -> MessageReport {
        let facts = match facts::load_facts(&self.db, message_id, need_headers).await {
            Ok(facts) => facts,
            Err(error) => {
                return MessageReport {
                    message_id,
                    rfc_message_id: None,
                    subject: String::new(),
                    from: String::new(),
                    rules: Vec::new(),
                    error: Some(error.to_string()),
                }
            }
        };
        if facts.account_id != account_id {
            // Scoping is enforced here, not only in the caller's query: a
            // message id is a client-supplied integer, and evaluating another
            // account's mail would classify it under this account's policy.
            return MessageReport {
                message_id,
                rfc_message_id: facts.rfc_message_id,
                subject: facts.subject,
                from: facts.from,
                rules: Vec::new(),
                error: Some(format!(
                    "message {message_id} does not belong to account {account_id}"
                )),
            };
        }

        let mut reports = Vec::with_capacity(rules.len());
        let mut error = None;
        for (rule_id, compiled) in rules {
            match self
                .evaluate_rule(*rule_id, compiled, &facts, dry_run, cancel)
                .await
            {
                Ok(report) => reports.push(report),
                Err(e) => {
                    // Recorded once, for the message, and the remaining rules
                    // are still evaluated: a provider outage should not hide
                    // what the deterministic rules would have done.
                    error.get_or_insert_with(|| e.to_string());
                    reports.push(RuleReport {
                        rule: compiled.spec.name.clone(),
                        matched: false,
                        outcomes: Vec::new(),
                        actions: Vec::new(),
                        already_fired: false,
                        model_called: false,
                        cache_hit: false,
                        explanation: None,
                    });
                }
            }
        }

        MessageReport {
            message_id,
            rfc_message_id: facts.rfc_message_id.clone(),
            subject: facts.subject.clone(),
            from: facts.from.clone(),
            rules: reports,
            error,
        }
    }

    async fn evaluate_rule(
        &self,
        rule_id: Option<i64>,
        compiled: &Compiled,
        facts: &MessageFacts,
        dry_run: bool,
        cancel: &CancellationToken,
    ) -> Result<RuleReport, Error> {
        let evaluation = eval::evaluate(compiled, facts, self.classifier.as_ref(), cancel).await?;
        let explanation = evaluation.explanation().map(ToOwned::to_owned);

        let mut report = RuleReport {
            rule: evaluation.rule,
            matched: evaluation.matched,
            outcomes: evaluation.outcomes,
            actions: Vec::new(),
            already_fired: false,
            model_called: evaluation.model_called,
            cache_hit: evaluation.cache_hit,
            explanation,
        };
        if !report.matched {
            return Ok(report);
        }

        if dry_run {
            report.already_fired = match rule_id {
                Some(id) => repo::already_fired(&self.db, id, facts.message_id).await?,
                None => false,
            };
            report.actions = self.actions.describe(&compiled.spec.then, facts).await?;
            return Ok(report);
        }

        // Claim before acting — see the module docs. An ad-hoc rule has no row
        // to claim against and therefore never fires; that is deliberate, and
        // the reason `BacktestRule` is a dry run by construction rather than
        // by a flag someone could pass wrong.
        let Some(id) = rule_id else {
            report.already_fired = false;
            report.actions = self.actions.describe(&compiled.spec.then, facts).await?;
            return Ok(report);
        };
        if !repo::claim(&self.db, id, facts.message_id).await? {
            report.already_fired = true;
            return Ok(report);
        }
        report.actions = self
            .actions
            .apply(&compiled.spec.name, &compiled.spec.then, facts, cancel)
            .await;
        Ok(report)
    }

    /// Run `selector` over every message in `account_id` from the last
    /// `days` days, as a dry run: prd.md #50's backtest.
    ///
    /// Bounded by the engine's `max_window` — a backtest is an interactive
    /// question, and one over a mailbox's whole history is a different
    /// (batch) feature.
    ///
    /// # Errors
    /// As [`RuleEngine::evaluate`].
    #[tracing::instrument(skip(self, selector, cancel), fields(account_id, days), err)]
    pub async fn backtest(
        &self,
        account_id: i64,
        selector: &RuleSelector,
        days: u32,
        cancel: &CancellationToken,
    ) -> Result<EvaluationReport, Error> {
        let since = chrono::Utc::now().timestamp() - i64::from(days) * 86_400;
        let message_ids = facts::window(&self.db, account_id, since, self.max_window).await?;
        self.evaluate(account_id, &message_ids, selector, true, cancel)
            .await
    }

    /// Record a user correction for a `claude_is` predicate, which becomes a
    /// few-shot example for every later classification of that predicate —
    /// and, for this message specifically, the authoritative answer.
    ///
    /// Returns how many corrections now exist for the predicate.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for an empty predicate,
    /// [`Error::NotFound`] if the message does not exist or belongs to
    /// another account.
    #[tracing::instrument(skip(self, prompt), fields(account_id, message_id, expected), err)]
    pub async fn record_correction(
        &self,
        account_id: i64,
        message_id: i64,
        prompt: &str,
        expected: bool,
    ) -> Result<i64, Error> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(Error::invalid_argument(
                "a correction must name the claude_is predicate it corrects",
            ));
        }
        if prompt.chars().count() > model::MAX_CLAUDE_IS_LEN {
            return Err(Error::invalid_argument(format!(
                "the corrected predicate must be at most {} characters",
                model::MAX_CLAUDE_IS_LEN
            )));
        }
        let facts = facts::load_facts(&self.db, message_id, false).await?;
        if facts.account_id != account_id {
            return Err(Error::not_found(format!(
                "message {message_id} in account {account_id}"
            )));
        }
        // Policy first, before a single byte of this message is copied
        // anywhere: see the `policy` field's own docs for what a correction
        // recorded on a forbidden folder would otherwise leak.
        let account = repo::account_name(&self.db, account_id)
            .await?
            .ok_or_else(|| Error::not_found(format!("account {account_id}")))?;
        let decision = self
            .policy
            .resolve(&PolicyTarget::account(account).mailbox(facts.mailbox.clone()));
        if !decision.is_visible() || !decision.permits_network() {
            return Err(Error::failed_precondition(format!(
                "ai policy resolved {:?} for this account/folder; a correction cannot be \
                 recorded from a message whose content may not reach a model, because every \
                 later classification of this predicate would replay it",
                decision.mode
            )));
        }
        repo::record_example(
            &self.db,
            account_id,
            prompt,
            message_id,
            // Frozen exactly as the classifier renders it, so the example
            // teaches the same text the model was originally shown.
            &facts.render_for_model(classify::MAX_BODY_CHARS),
            expected,
        )
        .await?;
        repo::example_count(&self.db, account_id, prompt).await
    }
}

/// The background consumer that evaluates rules on each new message.
///
/// Shaped exactly like [`crate::hooks::HookDispatcher`] and
/// [`crate::smart_folder::SmartFolderEvaluator`]: a tick that re-reads
/// [`EventLog::since`] from its own cursor rather than holding a live
/// subscription, for the reason those modules give — nobody is waiting
/// synchronously, and the durability guarantee is identical either way.
///
/// # The cursor starts at the head, never at the beginning of retention
///
/// This is [`crate::hooks::HookDispatcher`]'s rule, and it applies here for
/// the same reason and more strongly. A rule's actions have no dedup at the
/// event level — the `rule_actions_fired` claim is per `(rule, message)`, so
/// replaying history would *not* re-fire for messages already acted on, but a
/// restart would still re-evaluate every message in the retention window,
/// which for a rule with a `claude_is` means paying for a classification of
/// every one of them. [`RuleEvaluator::spawn`] seeds the cursor at
/// [`EventLog::latest_seq`] before it returns, so a restart only ever
/// considers mail that arrives from that moment on.
///
/// Mail that arrived while the daemon was down is therefore *not* evaluated
/// automatically. That is a deliberate trade, not an oversight: catching up
/// is `mail rule run --since 7d` (an `EvaluateRules` call over a window),
/// where a human is present to see the cost.
#[derive(Debug)]
pub struct RuleEvaluator {
    engine: RuleEngine,
    events: EventLog,
    cursor: AtomicI64,
    tick_interval: Duration,
    max_batch: usize,
}

/// What one [`RuleEvaluator::tick`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Messages evaluated.
    pub messages: usize,
    /// (message, rule) pairs that matched.
    pub matches: usize,
    /// Actions applied.
    pub actions_applied: usize,
    /// Provider calls made.
    pub model_calls: usize,
}

impl RuleEvaluator {
    /// Sentinel for "no cursor yet", distinct from position 0 ("everything
    /// the log has"), which is a real and different instruction.
    const UNSEEDED_CURSOR: i64 = -1;

    /// Build an evaluator over `engine`, following `events`.
    #[must_use]
    pub fn new(engine: RuleEngine, events: EventLog) -> Self {
        Self {
            engine,
            events,
            cursor: AtomicI64::new(Self::UNSEEDED_CURSOR),
            tick_interval: DEFAULT_TICK_INTERVAL,
            max_batch: DEFAULT_MAX_BATCH,
        }
    }

    /// Override the tick interval, floored at [`MIN_TICK_INTERVAL`].
    #[must_use]
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval.max(MIN_TICK_INTERVAL);
        self
    }

    /// Override how many messages one tick evaluates.
    #[must_use]
    pub fn with_max_batch(mut self, max_batch: usize) -> Self {
        self.max_batch = max_batch.max(1);
        self
    }

    /// Read the log forward and evaluate every account's enabled rules
    /// against the messages that arrived.
    ///
    /// # Errors
    /// A mapped storage error from reading the event log. A retention gap
    /// resets the cursor to the head and returns an empty report rather than
    /// replaying — see the type's own docs.
    #[tracing::instrument(skip(self, cancel), err)]
    pub async fn tick(&self, cancel: &CancellationToken) -> Result<TickReport, Error> {
        let mut cursor = self.cursor.load(Ordering::SeqCst);
        if cursor == Self::UNSEEDED_CURSOR {
            cursor = self.events.latest_seq().await?.unwrap_or(0);
        }

        // Grouped by account so one `resolve` (and one rule compilation)
        // serves every message that arrived for it this tick.
        let mut pending: std::collections::BTreeMap<i64, Vec<i64>> =
            std::collections::BTreeMap::new();
        let mut queued = 0usize;
        loop {
            let page = match self.events.since(cursor, DRAIN_PAGE).await {
                Ok(page) => page,
                Err(error) if error.reason() == ErrorReason::OutOfRange => {
                    let head = self.events.latest_seq().await?.unwrap_or(0);
                    tracing::warn!(
                        cursor,
                        head,
                        %error,
                        "the rule evaluator's cursor fell behind the event log's retention \
                         window; jumping to the head rather than re-classifying history"
                    );
                    self.cursor.store(head, Ordering::SeqCst);
                    return Ok(TickReport::default());
                }
                Err(error) => return Err(error),
            };
            let got = page.events.len();
            for event in &page.events {
                if event.kind != EventKind::NewMail {
                    continue;
                }
                if let (Some(account_id), Some(message_id)) = (event.account_id, event.message_id) {
                    pending.entry(account_id).or_default().push(message_id);
                    queued += 1;
                }
            }
            cursor = page.next_seq;
            // Checked at a page boundary, so the cursor never lands partway
            // through a page whose remaining events would then be skipped.
            if queued >= self.max_batch || i64::try_from(got).unwrap_or(i64::MAX) < DRAIN_PAGE {
                break;
            }
        }
        self.cursor.store(cursor, Ordering::SeqCst);

        let mut report = TickReport::default();
        for (account_id, message_ids) in pending {
            if cancel.is_cancelled() {
                break;
            }
            match self
                .engine
                .evaluate(
                    account_id,
                    &message_ids,
                    &RuleSelector::AllEnabled,
                    false,
                    cancel,
                )
                .await
            {
                Ok(evaluation) => {
                    report.messages += evaluation.messages.len();
                    report.matches += evaluation.matches;
                    report.actions_applied += evaluation.actions_applied;
                    report.model_calls += evaluation.model_calls;
                }
                // One account's rules failing (a rule that no longer
                // compiles, a storage fault) must not stop the others, and
                // must not wedge the cursor — which has already advanced, so
                // the next tick moves on rather than retrying forever.
                Err(error) => tracing::warn!(
                    account_id,
                    %error,
                    "rule evaluation failed for this account"
                ),
            }
        }
        Ok(report)
    }

    /// Seed the cursor at the log's head, then tick until `cancel` fires.
    ///
    /// Seeding *before* the task is spawned rather than on the first tick is
    /// what makes "the head" mean boot — the correction
    /// [`crate::hooks::HookDispatcher::spawn`] documents.
    pub async fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        match self.events.latest_seq().await {
            Ok(head) => self.cursor.store(head.unwrap_or(0), Ordering::SeqCst),
            Err(error) => tracing::warn!(
                %error,
                "could not read the event log head; the rule evaluator will seed its cursor \
                 on its first tick instead"
            ),
        }
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(self.tick_interval) => {}
                }
                match self.tick(&cancel).await {
                    Ok(report) => {
                        if report.messages > 0 {
                            tracing::debug!(?report, "rule evaluation tick");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "rule evaluation tick failed"),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
