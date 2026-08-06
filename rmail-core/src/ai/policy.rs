//! The per-account/folder AI policy and data-residency engine.
//!
//! [`PolicyEngine`] is the seam every other AI-facing task in this pipeline
//! consults before doing anything: [`crate::ai::redact`] before it decides
//! whether a body may even be tokenized for a call, the eventual worker queue
//! (task 47) before it admits a message, the budget enforcer (task 76) before
//! it decides which model tier a request may use, and the local-only model
//! path (task 78) to know which mail it — and only it — is allowed to touch.
//! None of those call `Provider::complete` directly on the strength of
//! `ai.enabled` alone; they call [`PolicyEngine::resolve`] first and act on
//! the [`PolicyDecision`] it returns. `ai/mod.rs`'s module docs call this out
//! explicitly: `provider::build` deliberately does not consult `ai.enabled`
//! because that is policy, not transport, and this module is where policy
//! lives.
//!
//! # What a resolution is
//!
//! Every account, folder, or (account, folder) pair resolves to a
//! [`PolicyDecision`]: an [`AiPolicyMode`] (`Allowed` | `LocalOnly` |
//! `Forbidden`) plus a freeform residency tag. The tag is never assumed to
//! mean anything (`"eu"`, `"on-device"`, `"unspecified"` are all just
//! strings this engine passes through) — interpreting it is a downstream
//! concern (e.g. task 78 deciding whether a tag names a jurisdiction the
//! configured cloud provider may not serve).
//!
//! # Precedence — read this before adding a rule
//!
//! A resolution walks the following checks *in order* and stops at the first
//! one that produces an answer:
//!
//! 1. **Global kill switch** (`ai.enabled = false`). Unconditional —
//!    `Forbidden` for every account and folder, full stop. No rule below can
//!    ever be reached while this is set; there is no reason a per-folder
//!    exception should survive turning AI off for the whole daemon.
//! 2. **Account hard opt-out** (`accounts.ai.enabled = false`, the field that
//!    already existed before this module — see `config::AccountAiConfig`).
//!    Also unconditional, but scoped to one account: `Forbidden` for every
//!    folder in that account, and *not* overridable by a more specific
//!    folder or pattern rule. This mirrors the PRD's framing of the field as
//!    a hard opt-out ("nothing leaves the machine") rather than a mere
//!    default — a folder-level exception quietly reopening an account a user
//!    explicitly shut off would defeat the point of the switch.
//! 3. **Folder rule** — an exact `(account?, folder)` match in
//!    `ai.policy.rules` (a `folder` value with no `*`/`?`). The most specific
//!    tier below the two unconditional checks above.
//! 4. **Pattern rule** — a glob `folder` value (`*`/`?`) matching the
//!    target's folder, optionally scoped to one account.
//! 5. **Account rule** — an `ai.policy.rules` entry naming only `account`
//!    (no `folder`).
//! 6. **Default** (`ai.policy.default_mode` / `default_residency`) — nothing
//!    above matched.
//!
//! Tiers 3–5 are tried in that order and **the first tier with any matching
//! rule wins outright** — a match at a less specific tier is never even
//! evaluated once a more specific one has an answer. This is the
//! *most-specific-match* property: an account-wide `local_only` rule loses
//! to a `Legal/*` pattern rule for that account, which in turn loses to an
//! exact `Legal/Privileged` folder rule.
//!
//! `accounts.ai.residency` (set without `accounts.ai.enabled = false`) is
//! **not** one of these tiers and never enters the mode contest at all — see
//! "Residency is resolved independently of mode" below for why, and for
//! where it actually applies.
//!
//! Within a single tier, more than one rule can legitimately match at once
//! (two overlapping patterns, say). When that happens this engine picks the
//! **most restrictive** classification among them
//! (`Forbidden` > `LocalOnly` > `Allowed` — see the `Ord` derived on
//! [`AiPolicyMode`] in `crate::config`, in that order deliberately) rather
//! than "first rule wins" or "last rule wins". This is the *deny-wins*
//! property: ambiguous, overlapping configuration never silently resolves to
//! the more permissive of two things an admin wrote down.
//!
//! Scoping a rule to one account does **not** make it more specific *within*
//! a tier — a global `folder = "Legal"` rule and an account-scoped
//! `account = "Personal", folder = "Legal"` rule are peers at the same
//! (folder) tier for `Personal`, and deny-wins picks the more restrictive of
//! the two regardless of which one names an account. Carving an exception
//! out of a broader rule requires moving to a genuinely more specific
//! *tier* (e.g. an exact-folder rule beats the pattern that would otherwise
//! cover it), not merely adding a narrower scope at the same tier.
//!
//! A glob is matched as written: `folder = "Legal/*"` matches `Legal/Inbox`
//! and `Legal/2024/Q1` but **not** `Legal` itself (`*` only ever matches
//! within the anchored pattern it is part of). Write a second rule for the
//! parent folder if it needs the same classification.
//!
//! Account and folder names are matched **byte-exact**, the same strings
//! `mailboxes.name` stores verbatim (see `storage::schema`) — no
//! case-folding, no hierarchy-delimiter normalization (`/` vs `.`). A rule
//! must name a folder exactly as the server reports it.
//!
//! # Residency is resolved independently of mode
//!
//! Mode and residency answer different questions, are governed by different
//! configuration (`ai.policy.rules[].mode` vs. `[].residency` and
//! `accounts.ai.residency`), and are walked *separately*:
//!
//! 1. At the tier that decided the mode (folder, pattern, or account),
//!    **only a rule that agrees with the winning mode** may supply the
//!    residency tag. A rule deny-wins outvoted must not still lend the
//!    decision its own tag — `{mode = "allowed", residency = "us"}` losing
//!    to `{mode = "local_only", residency = "eu"}` at the same tier resolves
//!    `LocalOnly` + `"eu"`, never `"us"` from the rule this engine rejected
//!    as less restrictive. If none of the winning-mode rules at that tier
//!    set a residency, the tier contributes nothing — it does not fall back
//!    to a losing rule's tag.
//! 2. Every tier *less specific* than the one that decided the mode never
//!    ran a mode contest of its own (a more specific tier already won), so
//!    there is no "winning rule" there to align with — the first rule
//!    (declaration order) at that tier that sets a residency simply applies.
//!    This is why a folder-level rule that changes the mode without naming a
//!    `residency` does not erase an account-level tag: the account tier is
//!    still consulted for residency even though it never got to decide mode.
//! 3. `accounts.ai.residency` is consulted **last**, right before
//!    `default_residency` — after every tier above has had a chance and
//!    found nothing. It is deliberately *not* one of the tiers above and
//!    never enters the mode contest: an earlier version of this engine
//!    folded it in as a synthesized `AiPolicyMode::Allowed` account-tier
//!    rule, which meant tagging an account `"eu"` for compliance reasons
//!    could silently *win* that account's mode outright whenever
//!    `ai.policy.default_mode` was configured more restrictively than
//!    `Allowed` — a residency annotation accidentally turning cloud AI *on*.
//!    Keeping it out of the mode contest entirely, and consulting it only
//!    for residency, closes that off structurally rather than by convention.
//!
//! # Why the default is `Allowed`, and why that is still the safe choice
//!
//! `ai.policy.default_mode` ships as [`AiPolicyMode::Allowed`]. That is a
//! deliberate choice, not an oversight, made for three reasons:
//!
//! - It matches the product's actual shipped behavior (PRD III-2): AI
//!   processing runs automatically on new mail by default. A policy engine
//!   whose empty configuration silently disabled that would contradict every
//!   other AI task's assumption that the pipeline works out of the box, and
//!   would make this engine something a user has to fight rather than a
//!   guardrail they opt into.
//! - The mandatory PII redaction firewall (task 44) sits between *every*
//!   `Allowed` resolution and an actual outbound call — this engine answers
//!   "is this mail eligible for AI at all", not "what may be sent verbatim".
//!   Defaulting to `Forbidden` would duplicate a protection that already
//!   exists structurally one stage later, at the cost of breaking AI by
//!   default for every account until a user explicitly allow-lists it.
//! - Where "safe" actually matters — a folder the user *has* flagged as
//!   sensitive — the precedence rules above guarantee that classification
//!   always wins over the default, and conflicting classifications always
//!   resolve toward the more restrictive one (deny-wins). Safety in this
//!   design lives in "an explicit boundary is never silently crossed", not
//!   in "nothing works until configured".
//!
//! `default_residency` ships as `"unspecified"` rather than guessing a
//! region — a caller that cares about residency (task 78) must treat
//! `"unspecified"` as "unknown", never as "compliant".
//!
//! # Forbidden means invisible, not merely denied
//!
//! A `Forbidden` resolution is not something calling code checks per message
//! and then declines to act on — that would still let a forbidden folder's
//! mail show up in an AI-facing listing (subject line, sender, existence)
//! even if no model call is made on it, which is exactly the leak this task
//! exists to close. The fix has to happen *before* a message-level query
//! runs, not after: [`PolicyEngine::visible_mailboxes`] is meant to be
//! called once, against an account's small, closed folder list (from
//! `mailboxes` — dozens of rows, not thousands), to get the subset of
//! folders AI features may ever touch. A caller (the queue's admission scan,
//! `StreamEnrichments`, semantic retrieval, `ask_mailbox`) turns that into a
//! `mailbox_id` allow-list and pushes it into the *message* query itself
//! (`WHERE mailbox_id IN (...)`) — that is what makes a forbidden folder's
//! mail structurally absent from the result set, not fetched, not scored,
//! not present to be filtered out later by a caller that might forget to
//! check. [`PolicyEngine::filter_visible`] is the general-purpose version of
//! the same idea for any `T`; using it *after* a message-level fetch has
//! already happened is a plain denial-list, not the structural property this
//! module promises — apply it to the folder list, or another small,
//! enumerable candidate set, before the expensive query runs.
//!
//! # Every resolution is logged, and always explainable
//!
//! [`PolicyEngine::resolve`] is a thin wrapper over [`PolicyEngine::explain`]
//! — every call to either logs the resolved target, mode, residency, and
//! matched tier. Most resolutions log at `tracing::debug!` (this fires once
//! per AI-eligible message on every AI path, the same hot-path-logs-at-debug
//! convention `index::queue` uses for its per-job events) — but a
//! `Forbidden` resolution logs at `tracing::info!` instead, because a denial
//! is the security-relevant outcome an operator needs visible under the
//! default `info` filter (`telemetry::init`'s default), not one that only
//! shows up if someone happens to be running with `debug` on. `explain`
//! returns the full structured trace — which tier matched, every candidate
//! rule considered at that tier, and a human narrative — which is what
//! `AiPolicyService.Evaluate`/`mail ai policy explain` (a later task; no
//! proto surface is added by this one) will render.

use std::collections::HashMap;

use regex::Regex;

pub use crate::config::AiPolicyMode;
use crate::config::{AiPolicyRule, Config};
use crate::error::Error;

// ---------------------------------------------------------------------------
// Public vocabulary
// ---------------------------------------------------------------------------

/// What is being classified: an account, or an (account, folder) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyTarget {
    /// Account name, as declared in `[[accounts]] name = "..."`.
    pub account: String,
    /// Folder/mailbox name within the account. `None` resolves the
    /// account-wide policy (no folder-specific tiers are consulted).
    pub mailbox: Option<String>,
}

impl PolicyTarget {
    /// An account-wide target (no specific folder).
    #[must_use]
    pub fn account(account: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            mailbox: None,
        }
    }

    /// Narrow this target to a specific folder.
    #[must_use]
    pub fn mailbox(mut self, mailbox: impl Into<String>) -> Self {
        self.mailbox = Some(mailbox.into());
        self
    }
}

impl std::fmt::Display for PolicyTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.mailbox {
            Some(mailbox) => write!(f, "{}:{mailbox}", self.account),
            None => write!(f, "{}", self.account),
        }
    }
}

/// The resolved classification for a [`PolicyTarget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDecision {
    /// What AI processing this target may receive.
    pub mode: AiPolicyMode,
    /// The residency tag in force for this target (never empty — falls back
    /// to `ai.policy.default_residency`, `"unspecified"` by default).
    pub residency: String,
}

impl PolicyDecision {
    /// Whether AI features may act on this target at all. `false` only for
    /// [`AiPolicyMode::Forbidden`] — see the module docs for why that must
    /// mean structurally invisible, not merely "declined".
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.mode != AiPolicyMode::Forbidden
    }

    /// Whether this target may reach a network provider. `false` for both
    /// [`AiPolicyMode::LocalOnly`] and [`AiPolicyMode::Forbidden`] — a
    /// caller building a request should check this, not just match on
    /// `Allowed`, so a future `AiPolicyMode` variant fails closed rather than
    /// silently being treated as network-eligible.
    #[must_use]
    pub fn permits_network(&self) -> bool {
        self.mode == AiPolicyMode::Allowed
    }
}

/// Which precedence tier produced a [`PolicyDecision`]. See the module docs'
/// "Precedence" section for what each one means and the order they are
/// tried in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyTier {
    /// `ai.enabled = false` — unconditional, every target.
    GlobalDisabled,
    /// `accounts.ai.enabled = false` — unconditional within the account.
    AccountDisabled,
    /// An exact `(account?, folder)` rule.
    Folder,
    /// A glob `folder` rule.
    Pattern,
    /// An explicit `ai.policy.rules` entry naming only `account` (no
    /// `folder`). `accounts.ai.residency` never decides this tier — see the
    /// module docs' "Residency is resolved independently of mode" section.
    Account,
    /// Nothing matched; `ai.policy.default_mode`/`default_residency` applied.
    Fallback,
}

/// One rule that matched at the tier [`PolicyExplanation::tier`] settled on,
/// whether or not it was the most restrictive (and therefore winning) one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleMatch {
    /// Human-readable description of what this rule matched on, e.g.
    /// `` account="Work" folder="Legal" ``.
    pub scope: String,
    /// The classification this rule assigns.
    pub mode: AiPolicyMode,
    /// The residency tag this rule assigns, if any.
    pub residency: Option<String>,
    /// This rule's `reason`, if any.
    pub reason: Option<String>,
}

/// A full resolution trace: the decision, which tier produced it, every rule
/// that matched at that tier (deny-wins candidates included, not just the
/// winner), and a one-paragraph human explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyExplanation {
    /// What was resolved.
    pub target: PolicyTarget,
    /// The resolution.
    pub decision: PolicyDecision,
    /// Which precedence tier decided it.
    pub tier: PolicyTier,
    /// Every rule that matched at `tier` (empty for [`PolicyTier::GlobalDisabled`],
    /// [`PolicyTier::AccountDisabled`], and [`PolicyTier::Fallback`], which are
    /// not rule-tiered — see the module docs).
    pub candidates: Vec<RuleMatch>,
    /// Human-readable explanation, suitable for `mail ai policy explain`.
    pub narrative: String,
}

// ---------------------------------------------------------------------------
// Compiled rule buckets (private: the public surface is `resolve`/`explain`)
// ---------------------------------------------------------------------------

/// A rule matched on an exact folder name.
#[derive(Debug, Clone)]
struct FolderRule {
    account: Option<String>,
    folder: String,
    mode: AiPolicyMode,
    residency: Option<String>,
    reason: Option<String>,
}

/// A rule matched on a folder glob (`*`/`?`).
#[derive(Debug, Clone)]
struct PatternRule {
    account: Option<String>,
    pattern_source: String,
    pattern: Regex,
    mode: AiPolicyMode,
    residency: Option<String>,
    reason: Option<String>,
}

/// A rule matched on the whole account.
#[derive(Debug, Clone)]
struct AccountRule {
    account: String,
    mode: AiPolicyMode,
    residency: Option<String>,
    reason: Option<String>,
}

/// Common surface [`winning_mode`] and [`first_residency`] need from any of
/// the three rule kinds above, so the deny-wins and residency-cascade logic
/// is written once rather than three times.
trait ScopedRule {
    fn mode(&self) -> AiPolicyMode;
    fn residency(&self) -> Option<&str>;
    fn reason(&self) -> Option<&str>;
    fn scope(&self) -> String;
}

impl ScopedRule for FolderRule {
    fn mode(&self) -> AiPolicyMode {
        self.mode
    }
    fn residency(&self) -> Option<&str> {
        self.residency.as_deref()
    }
    fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
    fn scope(&self) -> String {
        match &self.account {
            Some(account) => format!("account={account:?} folder={:?}", self.folder),
            None => format!("folder={:?} (any account)", self.folder),
        }
    }
}

impl ScopedRule for PatternRule {
    fn mode(&self) -> AiPolicyMode {
        self.mode
    }
    fn residency(&self) -> Option<&str> {
        self.residency.as_deref()
    }
    fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
    fn scope(&self) -> String {
        match &self.account {
            Some(account) => format!("account={account:?} pattern={:?}", self.pattern_source),
            None => format!("pattern={:?} (any account)", self.pattern_source),
        }
    }
}

impl ScopedRule for AccountRule {
    fn mode(&self) -> AiPolicyMode {
        self.mode
    }
    fn residency(&self) -> Option<&str> {
        self.residency.as_deref()
    }
    fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
    fn scope(&self) -> String {
        format!("account={:?}", self.account)
    }
}

/// A raw [`AiPolicyRule`], sorted into the tier it belongs to.
enum RuleBucket {
    Folder(FolderRule),
    Pattern(PatternRule),
    Account(AccountRule),
}

/// Classify one raw config rule into the tier it belongs to.
///
/// # Errors
///
/// [`Error::FailedPrecondition`] if the rule names neither `account` nor
/// `folder` (indistinguishable from `ai.policy.default_mode`, so rejected
/// rather than silently accepted as a no-op), or if `folder` is a glob whose
/// translation is not a valid pattern. `FailedPrecondition`, not
/// `InvalidArgument`: both variants' messages reach a client verbatim (see
/// `error.rs`), so the choice is not about what crosses the boundary — it is
/// about whose fault the error is and whether resending fixes it.
/// `InvalidArgument` means *this request's* arguments were bad, fixable by
/// the caller sending something different; a malformed `ai.policy.rules`
/// entry is the operator's own configuration failing to build, true for
/// every request until someone edits the TOML and restarts, which is exactly
/// `FailedPrecondition`'s contract ("system not in the required state").
fn classify(rule: AiPolicyRule) -> Result<RuleBucket, Error> {
    let AiPolicyRule {
        account,
        folder,
        mode,
        residency,
        reason,
    } = rule;
    match folder {
        None => {
            let account = account.ok_or_else(|| {
                Error::failed_precondition(
                    "an ai.policy rule needs an `account`, a `folder`, or both — one naming \
                     neither classifies nothing and is indistinguishable from \
                     `ai.policy.default_mode`"
                        .to_owned(),
                )
            })?;
            Ok(RuleBucket::Account(AccountRule {
                account,
                mode,
                residency,
                reason,
            }))
        }
        Some(folder) if folder.contains(['*', '?']) => {
            let pattern = glob_to_regex(&folder)?;
            Ok(RuleBucket::Pattern(PatternRule {
                account,
                pattern_source: folder,
                pattern,
                mode,
                residency,
                reason,
            }))
        }
        Some(folder) => Ok(RuleBucket::Folder(FolderRule {
            account,
            folder,
            mode,
            residency,
            reason,
        })),
    }
}

/// Translate a shell-style glob (`*` = any run of characters, `?` = exactly
/// one — the same subset `filename:`/`tag:` search operators document
/// elsewhere in this codebase) into an anchored [`Regex`]. Every other
/// character is escaped literally, so a folder name containing a regex
/// metacharacter (`.`, `+`, …) still matches only itself. Deliberately no
/// `[...]` character classes: the module docs' `folder` field only documents
/// `*`/`?`, and supporting a glob syntax the docs do not mention is worse
/// than not supporting it — an admin who writes one gets a folder name that
/// silently matches nothing rather than the class they expected.
fn glob_to_regex(pattern: &str) -> Result<Regex, Error> {
    let mut out = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out.push('$');
    Regex::new(&out).map_err(|e| {
        Error::failed_precondition(format!(
            "ai.policy rule folder pattern {pattern:?} is not a valid glob: {e}"
        ))
    })
}

/// Whether a rule scoped to `rule_account` (`None` = every account) applies
/// to `target_account`.
fn account_matches(rule_account: Option<&str>, target_account: &str) -> bool {
    rule_account.map_or(true, |account| account == target_account)
}

/// Apply the deny-wins tie-break across every rule that matched at one tier:
/// the most restrictive [`AiPolicyMode`] wins. Returns `None` for an empty
/// `hits` — an empty tier produced no answer, which is a distinct outcome
/// from "matched and it happened to resolve permissively", so this is not
/// represented as a fallback decision itself; the caller moves on to the
/// next tier. Ties in restrictiveness are not broken here (the resulting
/// mode is identical either way — see [`first_residency`] for where
/// declaration order actually matters).
fn winning_mode<R: ScopedRule>(hits: &[&R]) -> Option<(AiPolicyMode, Vec<RuleMatch>)> {
    let mode = hits.iter().map(|rule| rule.mode()).max()?;
    let candidates = hits
        .iter()
        .map(|rule| RuleMatch {
            scope: rule.scope(),
            mode: rule.mode(),
            residency: rule.residency().map(str::to_owned),
            reason: rule.reason().map(str::to_owned),
        })
        .collect();
    Some((mode, candidates))
}

/// The residency tag of the first rule in `hits` (declaration order) that
/// sets one, or `None` if no rule at this tier does. For tiers *other* than
/// the one that decided the mode — those never ran a mode contest of their
/// own (a more specific tier already won), so there is no "winning rule" at
/// this tier to align with, and the first declared tag simply applies. See
/// [`winning_tier_residency`] for the tier that actually decided the mode.
fn first_residency<R: ScopedRule>(hits: &[&R]) -> Option<String> {
    hits.iter()
        .find_map(|rule| rule.residency())
        .map(str::to_owned)
}

/// The residency tag of the first rule in `hits` (declaration order) that
/// **both agrees with the winning `mode` and sets a residency**.
///
/// Deliberately narrower than [`first_residency`]: within the tier that
/// decided the mode, a rule deny-wins outvoted must not still lend the
/// decision its own residency tag. Given `{mode="allowed", residency="us"}`
/// losing to `{mode="local_only", residency="eu"}` at the same tier, the
/// resolution must be `LocalOnly` + `"eu"` — not `"us"` from the rule this
/// engine explicitly rejected as less restrictive. If no rule agreeing with
/// `mode` sets a residency, this tier contributes nothing (it does **not**
/// fall back to a losing rule's tag); the caller moves on to the next,
/// less-specific tier via [`first_residency`] instead.
fn winning_tier_residency<R: ScopedRule>(hits: &[&R], mode: AiPolicyMode) -> Option<String> {
    hits.iter()
        .filter(|rule| rule.mode() == mode)
        .find_map(|rule| rule.residency())
        .map(str::to_owned)
}

/// Build the human-readable explanation for a tiered (non-fallback,
/// non-hard-gate) resolution.
fn narrative_for(candidates: &[RuleMatch], mode: AiPolicyMode, tier_desc: &str) -> String {
    let winner = candidates.iter().find(|c| c.mode == mode);
    let base = match winner {
        Some(c) => format!(
            "{tier_desc} matched ({}), classifying this {}",
            c.scope,
            mode.as_str()
        ),
        None => format!("{tier_desc} matched, classifying this {}", mode.as_str()),
    };
    let reason = winner.and_then(|c| c.reason.as_deref());
    match (candidates.len() > 1, reason) {
        (true, Some(r)) => format!(
            "{base}: {r} (chosen as the most restrictive of {} matching rules)",
            candidates.len()
        ),
        (true, None) => format!(
            "{base} (chosen as the most restrictive of {} matching rules)",
            candidates.len()
        ),
        (false, Some(r)) => format!("{base}: {r}"),
        (false, None) => base,
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// The compiled, queryable AI policy. Build once from configuration (cheap
/// but not free — patterns compile to [`Regex`]) and share behind an `Arc`
/// across every AI worker/caller; `resolve`/`explain` are read-only and take
/// `&self`.
#[derive(Debug)]
pub struct PolicyEngine {
    ai_enabled: bool,
    default_mode: AiPolicyMode,
    default_residency: String,
    /// Accounts with `accounts.ai.enabled = false`, mapped to their residency
    /// override (if any) — see [`PolicyTier::AccountDisabled`].
    account_opt_outs: HashMap<String, Option<String>>,
    /// Accounts with `accounts.ai.residency` set (and *not* opted out),
    /// mapped to that tag. Deliberately **not** rules in `account_rules`:
    /// this tag must never enter the mode contest — see
    /// [`PolicyEngine::from_config`]'s doc comment for why folding it in as
    /// an `AiPolicyMode::Allowed` rule let a residency annotation silently
    /// escalate an account past a more restrictive `default_mode`/explicit
    /// rule. It is consulted only as the last step of the residency cascade
    /// in [`PolicyEngine::explain`].
    account_residency_tags: HashMap<String, String>,
    folder_rules: Vec<FolderRule>,
    pattern_rules: Vec<PatternRule>,
    account_rules: Vec<AccountRule>,
}

impl PolicyEngine {
    /// Build an engine directly from a rule set, with no account opt-outs
    /// folded in and AI enabled globally.
    ///
    /// Test-only (`#[cfg(test)]`): a `PolicyEngine` that cannot see
    /// `ai.enabled`/`accounts.ai.enabled` must never be reachable from
    /// production code, since those are the two *unconditional* gates this
    /// module exists to enforce (see the module docs' "Precedence"
    /// section) — a `new` that silently hardcodes them to "everything is
    /// on" is exactly the fail-open bug this module exists to prevent, if
    /// anything outside a test ever reached for it. Every real caller goes
    /// through [`PolicyEngine::from_config`], which folds both in. This
    /// exists so the tiered rule resolution (folder/pattern/account
    /// precedence, deny-wins, residency cascading) can be unit-tested
    /// without constructing a full [`Config`] for every case.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if a rule is malformed — see [`classify`].
    #[cfg(test)]
    pub fn new(
        rules: Vec<AiPolicyRule>,
        default_mode: AiPolicyMode,
        default_residency: impl Into<String>,
    ) -> Result<Self, Error> {
        Self::build(
            true,
            HashMap::new(),
            HashMap::new(),
            rules,
            default_mode,
            default_residency.into(),
        )
    }

    /// Build an engine from the full configuration: `ai.policy.rules` plus
    /// every account's `ai.enabled`/`ai.residency`, folded in per the
    /// precedence rules documented on this module.
    ///
    /// `accounts.ai.residency` becomes an entry in
    /// [`PolicyEngine::account_residency_tags`], **not** a synthesized
    /// `AiPolicyMode::Allowed` rule the way an earlier version of this
    /// method did it. That was a real bug: folding the tag into the mode
    /// contest at the account tier meant a residency annotation on an
    /// otherwise-unclassified account could *win* the account tier outright
    /// (as `Allowed`) whenever `ai.policy.default_mode` was configured more
    /// restrictively than `Allowed` — an operator tagging an account `"eu"`
    /// for compliance reasons would have accidentally turned cloud AI *on*
    /// for it. Residency and mode are independent questions (see the module
    /// docs); this method keeps them independent all the way down to how
    /// they are stored, not just how `explain` reads them back out.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if a configured rule is malformed, or if
    /// a rule names an `account` that is not in `cfg.accounts` — a typo'd
    /// account name in a `forbidden` rule must fail loudly at build time
    /// rather than silently protecting nothing.
    pub fn from_config(cfg: &Config) -> Result<Self, Error> {
        let known_accounts: std::collections::HashSet<&str> =
            cfg.accounts.iter().map(|a| a.name.as_str()).collect();
        for rule in &cfg.ai.policy.rules {
            if let Some(account) = rule.account.as_deref() {
                if !known_accounts.contains(account) {
                    return Err(Error::failed_precondition(format!(
                        "ai.policy rule names account {account:?}, which is not a configured \
                         account"
                    )));
                }
            }
        }

        let mut account_opt_outs = HashMap::new();
        let mut account_residency_tags = HashMap::new();
        for account in &cfg.accounts {
            if !account.ai.enabled {
                account_opt_outs.insert(account.name.clone(), account.ai.residency.clone());
            } else if let Some(residency) = &account.ai.residency {
                account_residency_tags.insert(account.name.clone(), residency.clone());
            }
        }
        Self::build(
            cfg.ai.enabled,
            account_opt_outs,
            account_residency_tags,
            cfg.ai.policy.rules.clone(),
            cfg.ai.policy.default_mode,
            cfg.ai.policy.default_residency.clone(),
        )
    }

    fn build(
        ai_enabled: bool,
        account_opt_outs: HashMap<String, Option<String>>,
        account_residency_tags: HashMap<String, String>,
        raw_rules: Vec<AiPolicyRule>,
        default_mode: AiPolicyMode,
        default_residency: String,
    ) -> Result<Self, Error> {
        let mut folder_rules = Vec::new();
        let mut pattern_rules = Vec::new();
        let mut account_rules = Vec::new();
        for rule in raw_rules {
            match classify(rule)? {
                RuleBucket::Folder(r) => folder_rules.push(r),
                RuleBucket::Pattern(r) => pattern_rules.push(r),
                RuleBucket::Account(r) => account_rules.push(r),
            }
        }
        Ok(Self {
            ai_enabled,
            default_mode,
            default_residency,
            account_opt_outs,
            account_residency_tags,
            folder_rules,
            pattern_rules,
            account_rules,
        })
    }

    /// Resolve `target`, logging the resolution. Equivalent to
    /// `self.explain(target).decision` — call [`PolicyEngine::explain`]
    /// directly when the trace (matched tier, candidate rules, narrative) is
    /// needed, e.g. for `mail ai policy explain`.
    #[must_use]
    pub fn resolve(&self, target: &PolicyTarget) -> PolicyDecision {
        self.explain(target).decision
    }

    /// Resolve `target` and return the full trace: which precedence tier
    /// decided the mode, every rule that matched at that tier, and a human
    /// narrative. See the module docs' "Precedence" and "Residency is
    /// resolved independently of mode" sections for the order tiers are
    /// tried in and why mode and residency do not share one answer.
    #[must_use]
    pub fn explain(&self, target: &PolicyTarget) -> PolicyExplanation {
        if !self.ai_enabled {
            return self.finalize(
                target,
                PolicyTier::GlobalDisabled,
                Vec::new(),
                AiPolicyMode::Forbidden,
                self.default_residency.clone(),
                "AI is disabled globally (`ai.enabled = false`); every account and folder \
                 resolves forbidden until it is turned back on."
                    .to_owned(),
            );
        }
        if let Some(residency_override) = self.account_opt_outs.get(&target.account) {
            let residency = residency_override
                .clone()
                .unwrap_or_else(|| self.default_residency.clone());
            return self.finalize(
                target,
                PolicyTier::AccountDisabled,
                Vec::new(),
                AiPolicyMode::Forbidden,
                residency,
                format!(
                    "account {:?} has AI disabled (`accounts.ai.enabled = false`) — a hard \
                     opt-out that no folder or pattern rule can override.",
                    target.account
                ),
            );
        }

        // Every tier's hit-set is computed up front, regardless of which one
        // ends up deciding the mode: residency search below walks the same
        // tiers independently and needs all three, not just the winner's.
        let folder_hits: Vec<&FolderRule> = target
            .mailbox
            .as_deref()
            .map(|mailbox| {
                self.folder_rules
                    .iter()
                    .filter(|r| {
                        account_matches(r.account.as_deref(), &target.account)
                            && r.folder == mailbox
                    })
                    .collect()
            })
            .unwrap_or_default();
        let pattern_hits: Vec<&PatternRule> = target
            .mailbox
            .as_deref()
            .map(|mailbox| {
                self.pattern_rules
                    .iter()
                    .filter(|r| {
                        account_matches(r.account.as_deref(), &target.account)
                            && r.pattern.is_match(mailbox)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let account_hits: Vec<&AccountRule> = self
            .account_rules
            .iter()
            .filter(|r| r.account == target.account)
            .collect();

        // Mode: the first (most specific) tier with any hit wins outright.
        let (tier, mode, candidates, tier_desc) =
            if let Some((mode, candidates)) = winning_mode(&folder_hits) {
                (PolicyTier::Folder, mode, candidates, "an exact folder rule")
            } else if let Some((mode, candidates)) = winning_mode(&pattern_hits) {
                (
                    PolicyTier::Pattern,
                    mode,
                    candidates,
                    "a folder pattern rule",
                )
            } else if let Some((mode, candidates)) = winning_mode(&account_hits) {
                (
                    PolicyTier::Account,
                    mode,
                    candidates,
                    "an account-level rule",
                )
            } else {
                (PolicyTier::Fallback, self.default_mode, Vec::new(), "")
            };

        // Residency: within the tier that decided the mode, only a rule
        // agreeing with that mode may supply the tag (`winning_tier_residency`
        // — a deny-wins loser must not still lend the decision its own tag).
        // Every tier below that one never ran a mode contest of its own (a
        // more specific tier already won), so it contributes via plain
        // declaration order instead (`first_residency`). The account's
        // `accounts.ai.residency` tag is consulted last, before the global
        // default — see the module docs' "Residency is resolved
        // independently of mode" section.
        let residency = match tier {
            PolicyTier::Folder => winning_tier_residency(&folder_hits, mode)
                .or_else(|| first_residency(&pattern_hits))
                .or_else(|| first_residency(&account_hits)),
            PolicyTier::Pattern => winning_tier_residency(&pattern_hits, mode)
                .or_else(|| first_residency(&account_hits)),
            PolicyTier::Account => winning_tier_residency(&account_hits, mode),
            // Unreachable here: `GlobalDisabled`/`AccountDisabled` already
            // returned above, and `Fallback` means no tier matched at all.
            PolicyTier::GlobalDisabled | PolicyTier::AccountDisabled | PolicyTier::Fallback => None,
        }
        .or_else(|| self.account_residency_tags.get(&target.account).cloned())
        .unwrap_or_else(|| self.default_residency.clone());

        let narrative = match tier {
            PolicyTier::Fallback => format!(
                "no account, folder, or pattern rule matched {target}; falling back to \
                 `ai.policy.default_mode` = {}.",
                self.default_mode.as_str()
            ),
            _ => narrative_for(&candidates, mode, tier_desc),
        };

        self.finalize(target, tier, candidates, mode, residency, narrative)
    }

    fn finalize(
        &self,
        target: &PolicyTarget,
        tier: PolicyTier,
        candidates: Vec<RuleMatch>,
        mode: AiPolicyMode,
        residency: String,
        narrative: String,
    ) -> PolicyExplanation {
        let decision = PolicyDecision { mode, residency };
        let narrative = format!("{narrative} (residency: {})", decision.residency);
        // A `Forbidden` resolution is the security-relevant outcome an
        // operator needs visible under the default `info` filter
        // (`telemetry::init`'s default) — everything else is the ordinary,
        // once-per-message hot path and stays at `debug`, the same
        // hot-path-logs-at-debug convention `index::queue` uses for its
        // per-job events.
        if decision.mode == AiPolicyMode::Forbidden {
            tracing::info!(
                account = %target.account,
                mailbox = ?target.mailbox,
                mode = decision.mode.as_str(),
                residency = %decision.residency,
                tier = ?tier,
                "ai policy resolved"
            );
        } else {
            tracing::debug!(
                account = %target.account,
                mailbox = ?target.mailbox,
                mode = decision.mode.as_str(),
                residency = %decision.residency,
                tier = ?tier,
                "ai policy resolved"
            );
        }
        PolicyExplanation {
            target: target.clone(),
            decision,
            tier,
            candidates,
            narrative,
        }
    }

    /// Whether `target` may be seen by any AI feature at all. `false` only
    /// for a `Forbidden` resolution.
    ///
    /// This alone denies a single, already-known target — it does not, by
    /// itself, make a forbidden folder's mail invisible in a listing. Build
    /// AI-facing listings and retrieval candidate sets through
    /// [`PolicyEngine::visible_mailboxes`]/[`PolicyEngine::filter_visible`]
    /// instead, applied to the folder list *before* a message-level query
    /// runs, so a forbidden folder's messages are never in the candidate set
    /// to begin with (see the module docs' "Forbidden means invisible"
    /// section).
    #[must_use]
    pub fn is_visible(&self, target: &PolicyTarget) -> bool {
        self.resolve(target).is_visible()
    }

    /// The subset of `mailboxes` (an account's folder names) that AI
    /// features may ever see. Call this once, against the account's full,
    /// small, closed folder list (from `mailboxes` — dozens of rows, not
    /// thousands) — a caller turns the result into a `mailbox_id` allow-list
    /// pushed into the *message*-level query itself, which is what makes a
    /// forbidden folder's mail structurally absent from a result set rather
    /// than fetched and then discarded. See the module docs' "Forbidden
    /// means invisible" section.
    #[must_use]
    pub fn visible_mailboxes<'a>(
        &self,
        account: &str,
        mailboxes: impl IntoIterator<Item = &'a str>,
    ) -> Vec<&'a str> {
        mailboxes
            .into_iter()
            .filter(|mailbox| self.is_visible(&PolicyTarget::account(account).mailbox(*mailbox)))
            .collect()
    }

    /// Filter `items` down to the ones whose target [`PolicyEngine::is_visible`]
    /// permits, preserving order. The general-purpose version of
    /// [`PolicyEngine::visible_mailboxes`] for any `T` — but the structural
    /// "invisible, not merely denied" property only holds when this runs
    /// over a small, enumerable candidate set (a folder list) *before* the
    /// expensive query, the same as that method. Calling it over an
    /// already-fetched message listing is a plain denial-list: the leak this
    /// module exists to close already happened by the time the messages were
    /// read out of storage.
    #[must_use]
    pub fn filter_visible<T>(
        &self,
        items: Vec<T>,
        target_of: impl Fn(&T) -> PolicyTarget,
    ) -> Vec<T> {
        items
            .into_iter()
            .filter(|item| self.is_visible(&target_of(item)))
            .collect()
    }
}

#[cfg(test)]
mod tests;
