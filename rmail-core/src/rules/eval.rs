//! Predicate evaluation: turning a [`Compiled`] rule plus a
//! [`MessageFacts`] snapshot into a verdict, with a per-predicate trace.
//!
//! # The cheap predicates run first, and that is the whole design
//!
//! prd.md #45 pairs deterministic matchers with a `claude_is` natural-language
//! predicate, and #46 asks the synthesizer to "prefer cheap deterministic
//! predicates." Neither is worth much unless evaluation itself honours the
//! same ordering: a rule reading `from = "@vendor\\.example$"` **and**
//! `claude_is = "an invoice"` should cost a model call only for mail that
//! actually came from that vendor, not for every message that arrives.
//!
//! [`evaluate`] therefore always resolves every deterministic predicate
//! first, and consults [`Classifier`] only when the deterministic set has not
//! already decided the rule:
//!
//! - Under [`MatchMode::All`], one failed deterministic predicate settles the
//!   rule as "no match" — the `claude_is` is left [`Outcome::skipped`].
//! - Under [`MatchMode::Any`], one *satisfied* deterministic predicate settles
//!   it as "match", and again the `claude_is` is skipped.
//!
//! A skipped predicate is reported as skipped rather than as "did not match",
//! because a backtest that could not tell those apart would attribute a
//! rule's misses to the model when the model was never asked. `rules::tests::
//! a_failed_cheap_predicate_means_the_model_is_never_asked` is the regression
//! proof, asserted against a classifier that counts its calls.
//!
//! # Evaluation is pure
//!
//! Nothing in this module reads the database, writes anything, or fires an
//! action. It takes a snapshot and a classifier and returns a verdict. That
//! is what lets a dry run and a real evaluation share one implementation with
//! no `if dry_run` inside it — the difference between them is entirely in
//! whether [`super::actions`] is called afterwards, which is a property a
//! reader can check by looking at one call site instead of auditing a
//! branchy evaluator.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::rules::facts::MessageFacts;
use crate::rules::model::{bounded, Compiled, MatchMode};

/// One `claude_is` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// Whether the message satisfies the natural-language predicate.
    pub verdict: bool,
    /// The model's one-line justification — what `BacktestRule` reports per
    /// `claude_is` decision.
    pub explanation: String,
    /// Whether this came from the `message-id + prompt-hash` cache rather
    /// than a fresh provider call. Reported so a backtest can show how much
    /// of its run was actually paid for.
    pub cached: bool,
    /// The model that produced the verdict (the cached one when `cached`).
    pub model: String,
}

/// Answers a `claude_is` predicate for one message.
///
/// A trait, not a concrete type, for two reasons that are both about
/// testability rather than pluggability: the real implementation
/// ([`super::classify::ClaudeClassifier`]) needs a provider, a policy engine,
/// a redaction pass, and an audit ledger, and none of that belongs in a test
/// that only wants to know whether a `size` predicate short-circuits.
#[async_trait]
pub trait Classifier: Send + Sync + std::fmt::Debug {
    /// Classify `facts` against the natural-language `prompt`.
    ///
    /// # Errors
    /// Whatever the backing provider/policy/budget path returns. Callers
    /// treat an error as "this rule could not be decided for this message"
    /// and never as "the rule did not match" — see [`evaluate`].
    async fn classify(
        &self,
        prompt: &str,
        facts: &MessageFacts,
        cancel: &CancellationToken,
    ) -> Result<Classification, Error>;
}

/// One predicate's contribution to a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Which predicate this is, e.g. `from`, `header.list-id`, `claude_is`.
    pub predicate: String,
    /// Whether it was consulted at all. `false` means the deterministic set
    /// had already decided the rule — see the module docs.
    pub evaluated: bool,
    /// Whether it matched. Always `false` when `evaluated` is `false`.
    pub matched: bool,
    /// Human-readable detail: the model's explanation for a `claude_is`, the
    /// reason a predicate was skipped, or `None` for a plain match/miss.
    pub detail: Option<String>,
}

impl Outcome {
    fn plain(predicate: impl Into<String>, matched: bool) -> Self {
        Self {
            predicate: predicate.into(),
            evaluated: true,
            matched,
            detail: None,
        }
    }

    fn skipped(predicate: impl Into<String>, why: impl Into<String>) -> Self {
        Self {
            predicate: predicate.into(),
            evaluated: false,
            matched: false,
            detail: Some(why.into()),
        }
    }
}

/// What one rule did for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evaluation {
    /// The rule's name.
    pub rule: String,
    /// Whether the rule matched.
    pub matched: bool,
    /// Every predicate, in evaluation order.
    pub outcomes: Vec<Outcome>,
    /// Whether a `claude_is` was answered by a fresh provider call.
    pub model_called: bool,
    /// Whether a `claude_is` was answered from the cache.
    pub cache_hit: bool,
}

impl Evaluation {
    /// The `claude_is` explanation, when the predicate was consulted.
    #[must_use]
    pub fn explanation(&self) -> Option<&str> {
        self.outcomes
            .iter()
            .find(|o| o.predicate == CLAUDE_IS && o.evaluated)
            .and_then(|o| o.detail.as_deref())
    }
}

/// The predicate name the `claude_is` outcome is reported under.
pub const CLAUDE_IS: &str = "claude_is";

/// Evaluate `rule` against `facts`.
///
/// `classifier` is consulted at most once, and only if the deterministic
/// predicates left the verdict open — see the module docs.
///
/// # Errors
/// Whatever [`Classifier::classify`] returns. A rule with no `claude_is`, or
/// one whose deterministic predicates already settled it, cannot fail.
pub async fn evaluate(
    rule: &Compiled,
    facts: &MessageFacts,
    classifier: &dyn Classifier,
    cancel: &CancellationToken,
) -> Result<Evaluation, Error> {
    let mut outcomes = deterministic(rule, facts);
    let mode = rule.spec.match_mode;
    let decided = match mode {
        MatchMode::All => outcomes.iter().any(|o| !o.matched).then_some(false),
        MatchMode::Any => outcomes.iter().any(|o| o.matched).then_some(true),
    };

    let Some(prompt) = rule.spec.when.claude_is.as_deref() else {
        // No natural-language predicate: the deterministic set is the whole
        // rule. An `All` rule with no predicates at all is impossible —
        // `RuleSpec::validate` rejects it — so `all()`/`any()` over an empty
        // set is unreachable here rather than merely unlikely.
        let matched = match mode {
            MatchMode::All => outcomes.iter().all(|o| o.matched),
            MatchMode::Any => outcomes.iter().any(|o| o.matched),
        };
        return Ok(Evaluation {
            rule: rule.spec.name.clone(),
            matched,
            outcomes,
            model_called: false,
            cache_hit: false,
        });
    };

    if let Some(matched) = decided {
        outcomes.push(Outcome::skipped(
            CLAUDE_IS,
            match mode {
                MatchMode::All => {
                    "a cheaper predicate did not match, so the rule cannot match; the model \
                     was not asked"
                }
                MatchMode::Any => {
                    "a cheaper predicate already matched, so the rule matches; the model was \
                     not asked"
                }
            },
        ));
        return Ok(Evaluation {
            rule: rule.spec.name.clone(),
            matched,
            outcomes,
            model_called: false,
            cache_hit: false,
        });
    }

    let classification = classifier.classify(prompt, facts, cancel).await?;
    outcomes.push(Outcome {
        predicate: CLAUDE_IS.to_owned(),
        evaluated: true,
        matched: classification.verdict,
        detail: Some(classification.explanation.clone()),
    });
    let matched = match mode {
        MatchMode::All => outcomes.iter().all(|o| o.matched),
        MatchMode::Any => outcomes.iter().any(|o| o.matched),
    };
    Ok(Evaluation {
        rule: rule.spec.name.clone(),
        matched,
        outcomes,
        model_called: !classification.cached,
        cache_hit: classification.cached,
    })
}

/// Resolve every deterministic predicate, in a fixed order (cheapest first:
/// integer comparisons, then set membership, then regexes over progressively
/// larger haystacks). The order is not observable in the verdict — `all`/`any`
/// do not care — but it is what the reported trace is ordered by, and it is
/// the order a future short-circuit within this function would want.
fn deterministic(rule: &Compiled, facts: &MessageFacts) -> Vec<Outcome> {
    let when = &rule.spec.when;
    let limits = &rule.limits;
    let mut outcomes = Vec::new();

    if let Some(min) = when.min_bytes {
        outcomes.push(Outcome::plain("min_bytes", facts.size >= min));
    }
    if let Some(max) = when.max_bytes {
        outcomes.push(Outcome::plain("max_bytes", facts.size <= max));
    }
    if !when.has_flags.is_empty() {
        outcomes.push(Outcome::plain(
            "has_flags",
            when.has_flags.iter().all(|f| facts.flags.contains(f)),
        ));
    }
    if !when.lacks_flags.is_empty() {
        outcomes.push(Outcome::plain(
            "lacks_flags",
            when.lacks_flags.iter().all(|f| !facts.flags.contains(f)),
        ));
    }
    if let Some(re) = &rule.from {
        outcomes.push(Outcome::plain(
            "from",
            re.is_match(bounded(&facts.from, limits)),
        ));
    }
    if let Some(re) = &rule.subject {
        outcomes.push(Outcome::plain(
            "subject",
            re.is_match(bounded(&facts.subject, limits)),
        ));
    }
    for (name, re) in &rule.header {
        // A header that appears more than once (Received, and legitimately
        // repeated List-* fields) matches if *any* occurrence matches.
        // Requiring all of them would make `Received` predicates useless and
        // is not what "the message has this header" means to anyone.
        let matched = facts
            .header_values(name)
            .iter()
            .any(|value| re.is_match(bounded(value, limits)));
        outcomes.push(Outcome::plain(format!("header.{name}"), matched));
    }
    if let Some(re) = &rule.body {
        outcomes.push(Outcome::plain(
            "body",
            re.is_match(bounded(&facts.body, limits)),
        ));
    }
    outcomes
}
