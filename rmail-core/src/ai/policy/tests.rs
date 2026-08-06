//! The properties the module docs promise: precedence (most-specific-match),
//! deny-wins on same-tier conflicts, the safe default posture, forbidden
//! invisibility, and that every resolution is both logged and explainable.
//
// A handful of tests build a [`Config`] from TOML to exercise
// `PolicyEngine::from_config` (the two unconditional gates — the global kill
// switch and an account's hard opt-out — only exist there, folded in from
// `ai.enabled`/`accounts.ai`). Those run inside a `figment::Jail` for the
// same reason `config::tests` does: `Config::from_toml_str` merges an env
// overlay, and ambient `RMAIL_*` vars must not leak into a hermetic test.
#![allow(clippy::result_large_err)]

use std::io;
use std::sync::{Arc, Mutex};

use figment::Jail;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

use super::*;
use crate::config::{AiPolicyRule, Config};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A minimal `ai.policy.rules` entry with no residency/reason.
fn rule(account: Option<&str>, folder: Option<&str>, mode: AiPolicyMode) -> AiPolicyRule {
    AiPolicyRule {
        account: account.map(str::to_owned),
        folder: folder.map(str::to_owned),
        mode,
        residency: None,
        reason: None,
    }
}

/// A rule with a residency tag and a reason, for explain-trace assertions.
fn rule_full(
    account: Option<&str>,
    folder: Option<&str>,
    mode: AiPolicyMode,
    residency: Option<&str>,
    reason: Option<&str>,
) -> AiPolicyRule {
    AiPolicyRule {
        account: account.map(str::to_owned),
        folder: folder.map(str::to_owned),
        mode,
        residency: residency.map(str::to_owned),
        reason: reason.map(str::to_owned),
    }
}

/// Convert any displayable error into a `figment::Error`, for use inside
/// `Jail::expect_with` closures (which must return `Result<(), figment::Error>`).
fn fe<E: std::fmt::Display>(err: E) -> figment::Error {
    figment::Error::from(err.to_string())
}

// ---------------------------------------------------------------------------
// Default posture
// ---------------------------------------------------------------------------

#[test]
fn nothing_matches_falls_back_to_the_configured_default() {
    let engine = PolicyEngine::new(Vec::new(), AiPolicyMode::Allowed, "unspecified").unwrap();
    let target = PolicyTarget::account("Personal").mailbox("INBOX");

    let decision = engine.resolve(&target);
    assert_eq!(decision.mode, AiPolicyMode::Allowed);
    assert_eq!(decision.residency, "unspecified");

    let explanation = engine.explain(&target);
    assert_eq!(explanation.tier, PolicyTier::Fallback);
    assert!(explanation.candidates.is_empty());
    assert!(
        explanation.narrative.contains("Personal:INBOX"),
        "narrative should name the target: {}",
        explanation.narrative
    );
}

#[test]
fn a_custom_default_mode_and_residency_are_honored() {
    let engine = PolicyEngine::new(Vec::new(), AiPolicyMode::LocalOnly, "on-device").unwrap();
    let decision = engine.resolve(&PolicyTarget::account("Anything"));
    assert_eq!(decision.mode, AiPolicyMode::LocalOnly);
    assert_eq!(decision.residency, "on-device");
}

#[test]
fn a_default_mode_of_forbidden_makes_every_unclassified_target_invisible() {
    let engine = PolicyEngine::new(Vec::new(), AiPolicyMode::Forbidden, "unspecified").unwrap();
    let target = PolicyTarget::account("Anything").mailbox("INBOX");
    assert_eq!(engine.resolve(&target).mode, AiPolicyMode::Forbidden);
    assert!(!engine.is_visible(&target));
}

// ---------------------------------------------------------------------------
// Precedence: most-specific-match
// ---------------------------------------------------------------------------

#[test]
fn account_rule_applies_to_every_folder_in_the_account() {
    let rules = vec![rule(Some("Work"), None, AiPolicyMode::LocalOnly)];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    assert_eq!(
        engine
            .resolve(&PolicyTarget::account("Work").mailbox("INBOX"))
            .mode,
        AiPolicyMode::LocalOnly
    );
    assert_eq!(
        engine
            .resolve(&PolicyTarget::account("Work").mailbox("Archive"))
            .mode,
        AiPolicyMode::LocalOnly
    );
    // A different account is untouched by a rule scoped to "Work".
    assert_eq!(
        engine
            .resolve(&PolicyTarget::account("Personal").mailbox("INBOX"))
            .mode,
        AiPolicyMode::Allowed
    );
}

#[test]
fn folder_rule_beats_account_rule() {
    let rules = vec![
        rule(Some("Work"), None, AiPolicyMode::LocalOnly),
        rule(Some("Work"), Some("Legal"), AiPolicyMode::Forbidden),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    // The exact folder rule wins for "Legal"...
    let legal = engine.explain(&PolicyTarget::account("Work").mailbox("Legal"));
    assert_eq!(legal.decision.mode, AiPolicyMode::Forbidden);
    assert_eq!(legal.tier, PolicyTier::Folder);

    // ...but every other folder in the account still falls through to the
    // account-wide rule, not the default.
    let other = engine.explain(&PolicyTarget::account("Work").mailbox("INBOX"));
    assert_eq!(other.decision.mode, AiPolicyMode::LocalOnly);
    assert_eq!(other.tier, PolicyTier::Account);
}

#[test]
fn pattern_rule_beats_account_rule() {
    let rules = vec![
        rule(Some("Work"), None, AiPolicyMode::Allowed),
        rule(Some("Work"), Some("Legal/*"), AiPolicyMode::Forbidden),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let contracts = engine.explain(&PolicyTarget::account("Work").mailbox("Legal/Contracts"));
    assert_eq!(contracts.decision.mode, AiPolicyMode::Forbidden);
    assert_eq!(contracts.tier, PolicyTier::Pattern);

    let inbox = engine.explain(&PolicyTarget::account("Work").mailbox("INBOX"));
    assert_eq!(inbox.decision.mode, AiPolicyMode::Allowed);
    assert_eq!(inbox.tier, PolicyTier::Account);
}

#[test]
fn folder_rule_beats_pattern_rule() {
    let rules = vec![
        rule(Some("Work"), Some("Legal/*"), AiPolicyMode::Forbidden),
        rule(
            Some("Work"),
            Some("Legal/Newsletter"),
            AiPolicyMode::Allowed,
        ),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    // The exact match is more specific than the pattern, even though the
    // pattern also matches and disagrees.
    let newsletter = engine.explain(&PolicyTarget::account("Work").mailbox("Legal/Newsletter"));
    assert_eq!(newsletter.decision.mode, AiPolicyMode::Allowed);
    assert_eq!(newsletter.tier, PolicyTier::Folder);

    // A sibling folder only the pattern covers still resolves via the pattern.
    let contracts = engine.explain(&PolicyTarget::account("Work").mailbox("Legal/Contracts"));
    assert_eq!(contracts.decision.mode, AiPolicyMode::Forbidden);
    assert_eq!(contracts.tier, PolicyTier::Pattern);
}

#[test]
fn pattern_rule_is_scoped_to_its_account_when_one_is_named() {
    let rules = vec![rule(Some("Work"), Some("Legal/*"), AiPolicyMode::Forbidden)];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    // "Personal" has a "Legal/Contracts" folder too, but the rule is scoped
    // to "Work" only.
    let personal = engine.resolve(&PolicyTarget::account("Personal").mailbox("Legal/Contracts"));
    assert_eq!(personal.mode, AiPolicyMode::Allowed);

    let work = engine.resolve(&PolicyTarget::account("Work").mailbox("Legal/Contracts"));
    assert_eq!(work.mode, AiPolicyMode::Forbidden);
}

#[test]
fn an_unscoped_pattern_rule_applies_across_every_account() {
    let rules = vec![rule(None, Some("Legal/*"), AiPolicyMode::Forbidden)];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    for account in ["Work", "Personal"] {
        assert_eq!(
            engine
                .resolve(&PolicyTarget::account(account).mailbox("Legal/Contracts"))
                .mode,
            AiPolicyMode::Forbidden,
            "account {account}"
        );
    }
}

#[test]
fn a_pattern_does_not_match_its_own_parent_folder() {
    // `Legal/*` matches children of "Legal" but not "Legal" itself — a
    // documented footgun, not a bug, but it must behave exactly this way.
    let rules = vec![rule(Some("Work"), Some("Legal/*"), AiPolicyMode::Forbidden)];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    assert_eq!(
        engine
            .resolve(&PolicyTarget::account("Work").mailbox("Legal/Contracts"))
            .mode,
        AiPolicyMode::Forbidden,
        "a child folder must match the pattern"
    );
    assert_eq!(
        engine
            .resolve(&PolicyTarget::account("Work").mailbox("Legal"))
            .mode,
        AiPolicyMode::Allowed,
        "the parent folder itself is not covered by `Legal/*`"
    );
}

#[test]
fn scoping_a_rule_to_one_account_does_not_win_a_same_tier_tie() {
    // An unscoped folder rule and an account-scoped folder rule for the same
    // folder are peers at the folder tier — deny-wins picks the more
    // restrictive of the two regardless of which one names an account, so
    // scoping alone cannot carve an allowed exception out of a global forbid
    // at the same tier (only a more specific *tier*, e.g. a pattern or exact
    // rule the global one does not also match, can do that).
    let rules = vec![
        rule(None, Some("Legal"), AiPolicyMode::Forbidden),
        rule(Some("Personal"), Some("Legal"), AiPolicyMode::Allowed),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let explanation = engine.explain(&PolicyTarget::account("Personal").mailbox("Legal"));
    assert_eq!(explanation.decision.mode, AiPolicyMode::Forbidden);
    assert_eq!(explanation.tier, PolicyTier::Folder);
    assert_eq!(explanation.candidates.len(), 2);
}

#[test]
fn glob_translation_treats_regex_metacharacters_in_the_pattern_literally() {
    let rules = vec![rule(
        Some("Work"),
        Some("Archive.2024*"),
        AiPolicyMode::Forbidden,
    )];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    // `.` in the pattern must match only a literal dot, not "any character" —
    // otherwise "ArchiveX2024-Q1" would wrongly match too.
    assert_eq!(
        engine
            .resolve(&PolicyTarget::account("Work").mailbox("Archive.2024-Q1"))
            .mode,
        AiPolicyMode::Forbidden
    );
    assert_eq!(
        engine
            .resolve(&PolicyTarget::account("Work").mailbox("ArchiveX2024-Q1"))
            .mode,
        AiPolicyMode::Allowed
    );
}

// ---------------------------------------------------------------------------
// Precedence: deny-wins on same-tier conflicts
// ---------------------------------------------------------------------------

#[test]
fn deny_wins_when_two_patterns_at_the_same_tier_disagree() {
    // Both match "Legal": one says allowed, the other forbidden.
    let rules = vec![
        rule(Some("Work"), Some("Le*"), AiPolicyMode::Allowed),
        rule(Some("Work"), Some("*gal"), AiPolicyMode::Forbidden),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let explanation = engine.explain(&PolicyTarget::account("Work").mailbox("Legal"));
    assert_eq!(explanation.decision.mode, AiPolicyMode::Forbidden);
    assert_eq!(explanation.tier, PolicyTier::Pattern);
    assert_eq!(
        explanation.candidates.len(),
        2,
        "both matching rules should be recorded as candidates, not just the winner"
    );
}

#[test]
fn deny_wins_ranks_local_only_above_allowed() {
    let rules = vec![
        rule(Some("Work"), Some("*"), AiPolicyMode::Allowed),
        rule(Some("Work"), Some("Inbox*"), AiPolicyMode::LocalOnly),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let decision = engine.resolve(&PolicyTarget::account("Work").mailbox("Inbox"));
    assert_eq!(decision.mode, AiPolicyMode::LocalOnly);
}

#[test]
fn a_tie_in_restrictiveness_breaks_toward_the_first_declared_rule() {
    // Both rules resolve to the same mode; only which rule's residency
    // "wins" is ambiguous, and declaration order settles it.
    let rules = vec![
        rule_full(
            Some("Work"),
            Some("Legal/*"),
            AiPolicyMode::Forbidden,
            Some("eu"),
            Some("first"),
        ),
        rule_full(
            Some("Work"),
            Some("*Contracts"),
            AiPolicyMode::Forbidden,
            Some("us"),
            Some("second"),
        ),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let explanation = engine.explain(&PolicyTarget::account("Work").mailbox("Legal/Contracts"));
    assert_eq!(explanation.decision.mode, AiPolicyMode::Forbidden);
    assert_eq!(explanation.decision.residency, "eu");
    assert!(explanation.narrative.contains("first"));
}

#[test]
fn a_same_tier_losing_rules_residency_does_not_attach_to_the_winning_mode() {
    // Two rules match at the same tier with *different* modes: deny-wins
    // picks `forbidden`, but the rule that lost that contest also set a
    // residency tag. That tag must not attach to the mode this engine
    // explicitly rejected as less restrictive — `winning_tier_residency`
    // only looks at rules that agree with the winning mode.
    let rules = vec![
        rule_full(
            Some("Work"),
            Some("Le*"),
            AiPolicyMode::Allowed,
            Some("us"),
            None,
        ),
        rule_full(
            Some("Work"),
            Some("*gal"),
            AiPolicyMode::Forbidden,
            Some("eu"),
            None,
        ),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let explanation = engine.explain(&PolicyTarget::account("Work").mailbox("Legal"));
    assert_eq!(explanation.decision.mode, AiPolicyMode::Forbidden);
    assert_eq!(
        explanation.decision.residency, "eu",
        "residency must come from a rule that agrees with the winning mode, not from the \
         allowed rule deny-wins rejected"
    );
}

#[test]
fn an_account_tier_rules_residency_survives_a_folder_rule_that_sets_none_of_its_own() {
    // The folder tier decides the mode and sets no residency; an *explicit*
    // account-tier rule (not the `accounts.ai.residency` tag map) carries
    // one. `first_residency` must still find it — this is the account tier
    // actually contributing a rule-based residency, not the synthesized-tag
    // fallback other tests exercise.
    let rules = vec![
        rule_full(Some("Work"), None, AiPolicyMode::Allowed, Some("us"), None),
        rule(Some("Work"), Some("Newsletters"), AiPolicyMode::Allowed),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let explanation = engine.explain(&PolicyTarget::account("Work").mailbox("Newsletters"));
    assert_eq!(explanation.decision.mode, AiPolicyMode::Allowed);
    assert_eq!(explanation.tier, PolicyTier::Folder);
    assert_eq!(explanation.decision.residency, "us");
}

#[test]
fn an_account_tier_rules_residency_survives_a_pattern_rule_that_sets_none_of_its_own() {
    // Same as above, but the mode is decided at the pattern tier instead of
    // the folder tier — exercises the `Pattern => ... first_residency(&account_hits)`
    // arm specifically.
    let rules = vec![
        rule_full(Some("Work"), None, AiPolicyMode::Allowed, Some("us"), None),
        rule(Some("Work"), Some("Legal/*"), AiPolicyMode::Allowed),
    ];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let explanation = engine.explain(&PolicyTarget::account("Work").mailbox("Legal/Contracts"));
    assert_eq!(explanation.decision.mode, AiPolicyMode::Allowed);
    assert_eq!(explanation.tier, PolicyTier::Pattern);
    assert_eq!(explanation.decision.residency, "us");
}

// ---------------------------------------------------------------------------
// Residency resolves independently of mode
// ---------------------------------------------------------------------------

#[test]
fn an_account_residency_tag_survives_a_more_specific_rule_that_sets_none_of_its_own() {
    // The mode is decided at the folder tier, which names no residency — the
    // account's `accounts.ai.residency` tag must still surface rather than
    // being erased just because a more specific tier won the mode contest.
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[accounts]]
name = "Eu-Only"
[accounts.ai]
enabled = true
residency = "eu"

[[ai.policy.rules]]
account = "Eu-Only"
folder = "Newsletters"
mode = "allowed"
"#,
        )
        .map_err(fe)?;
        let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

        let explanation = engine.explain(&PolicyTarget::account("Eu-Only").mailbox("Newsletters"));
        assert_eq!(explanation.decision.mode, AiPolicyMode::Allowed);
        assert_eq!(
            explanation.tier,
            PolicyTier::Folder,
            "mode decided at Folder"
        );
        assert_eq!(
            explanation.decision.residency, "eu",
            "residency must still come from the account tag even though the folder tier decided \
             the mode and set no residency of its own"
        );
        Ok(())
    });
}

#[test]
fn an_explicit_account_rule_residency_beats_the_account_tag_when_both_apply() {
    // The explicit `ai.policy.rules` account entry decides the mode and sets
    // its own residency; `accounts.ai.residency` is only ever consulted as a
    // last resort (the final `.or_else(account_residency_tags...)` step in
    // `explain`), so the rule's own tag wins outright — the account tag is
    // never even reached.
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[accounts]]
name = "Work"
[accounts.ai]
enabled = true
residency = "eu"

[[ai.policy.rules]]
account = "Work"
mode = "local_only"
residency = "us"
"#,
        )
        .map_err(fe)?;
        let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

        let explanation = engine.explain(&PolicyTarget::account("Work").mailbox("INBOX"));
        assert_eq!(explanation.decision.mode, AiPolicyMode::LocalOnly);
        assert_eq!(explanation.tier, PolicyTier::Account);
        assert_eq!(explanation.decision.residency, "us");
        Ok(())
    });
}

#[test]
fn the_account_residency_tag_still_applies_when_the_winning_rule_sets_none() {
    // The explicit account rule decides the mode but names no residency, and
    // it is the *only* rule at that tier — `winning_tier_residency` finds
    // nothing to align with, so the search falls through to the account's
    // `accounts.ai.residency` tag rather than straight to `default_residency`.
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[accounts]]
name = "Work"
[accounts.ai]
enabled = true
residency = "eu"

[[ai.policy.rules]]
account = "Work"
mode = "local_only"
"#,
        )
        .map_err(fe)?;
        let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

        let explanation = engine.explain(&PolicyTarget::account("Work").mailbox("INBOX"));
        assert_eq!(explanation.decision.mode, AiPolicyMode::LocalOnly);
        assert_eq!(explanation.decision.residency, "eu");
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Forbidden means invisible
// ---------------------------------------------------------------------------

#[test]
fn forbidden_folder_is_invisible_even_though_the_account_default_allows() {
    let rules = vec![rule(
        Some("Work"),
        Some("Legal/Privileged"),
        AiPolicyMode::Forbidden,
    )];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    assert!(!engine.is_visible(&PolicyTarget::account("Work").mailbox("Legal/Privileged")));
    assert!(engine.is_visible(&PolicyTarget::account("Work").mailbox("INBOX")));
}

#[test]
fn visible_mailboxes_excludes_forbidden_folders_before_any_message_query() {
    let rules = vec![rule(
        Some("Work"),
        Some("Legal/Privileged"),
        AiPolicyMode::Forbidden,
    )];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let all_folders = ["INBOX", "Legal/Privileged", "Legal/Contracts", "Archive"];
    let visible = engine.visible_mailboxes("Work", all_folders);

    assert_eq!(visible, vec!["INBOX", "Legal/Contracts", "Archive"]);
}

#[test]
fn filter_visible_removes_forbidden_items_from_a_listing_without_denying_them_individually() {
    let rules = vec![rule(
        Some("Work"),
        Some("Legal/Privileged"),
        AiPolicyMode::Forbidden,
    )];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    // Simulates an AI-facing listing: (account, mailbox, message subject).
    let listing = vec![
        ("Work", "INBOX", "quarterly numbers"),
        ("Work", "Legal/Privileged", "attorney-client memo"),
        ("Work", "Legal/Contracts", "vendor agreement"),
    ];

    let visible = engine.filter_visible(listing, |(account, mailbox, _subject)| {
        PolicyTarget::account(*account).mailbox(*mailbox)
    });

    let subjects: Vec<&str> = visible.iter().map(|(_, _, subject)| *subject).collect();
    assert_eq!(subjects, vec!["quarterly numbers", "vendor agreement"]);
    assert!(
        !subjects.contains(&"attorney-client memo"),
        "a forbidden folder's mail must not appear in an AI-facing listing at all"
    );
}

// ---------------------------------------------------------------------------
// The two unconditional gates (need `from_config`)
// ---------------------------------------------------------------------------

#[test]
fn global_kill_switch_forbids_everything_unconditionally() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[accounts]]
name = "Personal"

[ai]
enabled = false

[[ai.policy.rules]]
account = "Personal"
folder = "INBOX"
mode = "allowed"
"#,
        )
        .map_err(fe)?;
        let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

        // Even a rule that explicitly allows this exact target cannot survive
        // the global switch.
        let explanation = engine.explain(&PolicyTarget::account("Personal").mailbox("INBOX"));
        assert_eq!(explanation.decision.mode, AiPolicyMode::Forbidden);
        assert_eq!(explanation.tier, PolicyTier::GlobalDisabled);
        Ok(())
    });
}

#[test]
fn account_hard_opt_out_cannot_be_overridden_by_a_folder_rule() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[accounts]]
name = "Personal-Legal"
[accounts.ai]
enabled = false
residency = "us"

[[ai.policy.rules]]
account = "Personal-Legal"
folder = "Newsletters"
mode = "allowed"
"#,
        )
        .map_err(fe)?;
        let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

        let explanation =
            engine.explain(&PolicyTarget::account("Personal-Legal").mailbox("Newsletters"));
        assert_eq!(explanation.decision.mode, AiPolicyMode::Forbidden);
        assert_eq!(explanation.tier, PolicyTier::AccountDisabled);
        // The account's own residency tag still surfaces on a hard opt-out.
        assert_eq!(explanation.decision.residency, "us");
        Ok(())
    });
}

#[test]
fn account_residency_without_opt_out_applies_the_tag_without_touching_mode() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[accounts]]
name = "Eu-Only"
[accounts.ai]
enabled = true
residency = "eu"
"#,
        )
        .map_err(fe)?;
        let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

        let explanation = engine.explain(&PolicyTarget::account("Eu-Only").mailbox("INBOX"));
        // Mode falls through to the (default) `ai.policy.default_mode` —
        // nothing classified this account, the residency tag is not a mode
        // rule — while residency still picks up the account's tag.
        assert_eq!(explanation.decision.mode, AiPolicyMode::Allowed);
        assert_eq!(explanation.decision.residency, "eu");
        assert_eq!(explanation.tier, PolicyTier::Fallback);
        Ok(())
    });
}

#[test]
fn an_account_residency_tag_does_not_escalate_a_more_restrictive_default_mode() {
    // Regression test: an earlier version of this engine folded
    // `accounts.ai.residency` into the mode contest as a synthesized
    // `AiPolicyMode::Allowed` rule, so tagging an account for compliance
    // reasons could silently *win* the account's mode outright and grant
    // cloud AI access an operator never intended, whenever `default_mode`
    // was configured more restrictively than `Allowed`. The residency tag
    // must never change the mode, in either direction.
    for default_mode in [AiPolicyMode::LocalOnly, AiPolicyMode::Forbidden] {
        Jail::expect_with(|jail| {
            jail.clear_env();
            let cfg = Config::from_toml_str(&format!(
                r#"
[ai.policy]
default_mode = "{}"

[[accounts]]
name = "Eu-Only"
[accounts.ai]
enabled = true
residency = "eu"
"#,
                default_mode.as_str()
            ))
            .map_err(fe)?;
            let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

            let explanation = engine.explain(&PolicyTarget::account("Eu-Only").mailbox("INBOX"));
            assert_eq!(
                explanation.decision.mode, default_mode,
                "a residency tag alone must not change the resolved mode ({default_mode:?})"
            );
            assert_eq!(
                explanation.decision.residency, "eu",
                "the residency tag must still apply regardless of mode ({default_mode:?})"
            );
            assert_eq!(explanation.tier, PolicyTier::Fallback);
            Ok(())
        });
    }
}

#[test]
fn an_account_residency_tag_does_not_win_against_an_explicit_forbidding_rule() {
    // Same failure mode as above, but with an explicit rule instead of the
    // default: a folder the admin explicitly forbade must stay forbidden
    // even though the account carries a residency tag.
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[accounts]]
name = "Eu-Only"
[accounts.ai]
enabled = true
residency = "eu"

[[ai.policy.rules]]
account = "Eu-Only"
folder = "Legal"
mode = "forbidden"
"#,
        )
        .map_err(fe)?;
        let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

        let explanation = engine.explain(&PolicyTarget::account("Eu-Only").mailbox("Legal"));
        assert_eq!(explanation.decision.mode, AiPolicyMode::Forbidden);
        assert_eq!(explanation.decision.residency, "eu");
        assert_eq!(explanation.tier, PolicyTier::Folder);
        Ok(())
    });
}

#[test]
fn account_opt_out_only_affects_its_own_account() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[accounts]]
name = "Blocked"
[accounts.ai]
enabled = false

[[accounts]]
name = "Fine"
"#,
        )
        .map_err(fe)?;
        let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

        assert_eq!(
            engine
                .resolve(&PolicyTarget::account("Blocked").mailbox("INBOX"))
                .mode,
            AiPolicyMode::Forbidden
        );
        assert_eq!(
            engine
                .resolve(&PolicyTarget::account("Fine").mailbox("INBOX"))
                .mode,
            AiPolicyMode::Allowed
        );
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn a_rule_naming_neither_account_nor_folder_is_rejected() {
    let rules = vec![rule(None, None, AiPolicyMode::Forbidden)];
    let err = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified")
        .expect_err("a rule with no scope should be rejected");
    // FailedPrecondition, not InvalidArgument: this is the operator's own
    // config failing to build, and InvalidArgument's message reaches a gRPC
    // client verbatim — see `classify`'s doc comment.
    assert_eq!(err.reason(), crate::ErrorReason::FailedPrecondition);
}

#[test]
fn a_rule_naming_an_unconfigured_account_is_rejected() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[accounts]]
name = "Work"

[[ai.policy.rules]]
account = "Wrok"
folder = "Legal"
mode = "forbidden"
"#,
        )
        .map_err(fe)?;
        let err = PolicyEngine::from_config(&cfg)
            .expect_err("a typo'd account name should be rejected, not silently a no-op");
        assert_eq!(err.reason(), crate::ErrorReason::FailedPrecondition);
        assert!(
            err.to_string().contains("Wrok"),
            "error should name the offending account: {err}"
        );
        Ok(())
    });
}

#[test]
fn a_rule_naming_no_account_is_accepted_by_from_config_even_with_no_accounts_configured() {
    // `account: None` means "every account" and is valid on its own — this
    // must not be confused with the "neither account nor folder" rejection,
    // and it must not require any `[[accounts]]` to exist at all (a policy
    // rule can be written before the first account is even added).
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str(
            r#"
[[ai.policy.rules]]
folder = "Legal"
mode = "forbidden"
"#,
        )
        .map_err(fe)?;
        let engine = PolicyEngine::from_config(&cfg).map_err(fe)?;

        assert_eq!(
            engine
                .resolve(&PolicyTarget::account("AnyAccount").mailbox("Legal"))
                .mode,
            AiPolicyMode::Forbidden
        );
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// AiPolicyMode ordering (deny-wins depends on this)
// ---------------------------------------------------------------------------

#[test]
fn ai_policy_mode_orders_from_least_to_most_restrictive() {
    assert!(AiPolicyMode::Allowed < AiPolicyMode::LocalOnly);
    assert!(AiPolicyMode::LocalOnly < AiPolicyMode::Forbidden);
    assert_eq!(
        AiPolicyMode::Allowed.max(AiPolicyMode::Forbidden),
        AiPolicyMode::Forbidden
    );
}

// ---------------------------------------------------------------------------
// PolicyDecision helpers
// ---------------------------------------------------------------------------

#[test]
fn policy_decision_visibility_and_network_helpers_agree_with_mode() {
    let allowed = PolicyDecision {
        mode: AiPolicyMode::Allowed,
        residency: "unspecified".to_owned(),
    };
    let local = PolicyDecision {
        mode: AiPolicyMode::LocalOnly,
        residency: "unspecified".to_owned(),
    };
    let forbidden = PolicyDecision {
        mode: AiPolicyMode::Forbidden,
        residency: "unspecified".to_owned(),
    };

    assert!(allowed.is_visible() && allowed.permits_network());
    assert!(local.is_visible() && !local.permits_network());
    assert!(!forbidden.is_visible() && !forbidden.permits_network());
}

// ---------------------------------------------------------------------------
// Target formatting
// ---------------------------------------------------------------------------

#[test]
fn policy_target_display_includes_the_folder_only_when_set() {
    assert_eq!(PolicyTarget::account("Work").to_string(), "Work");
    assert_eq!(
        PolicyTarget::account("Work").mailbox("INBOX").to_string(),
        "Work:INBOX"
    );
}

// ---------------------------------------------------------------------------
// Explain narrative
// ---------------------------------------------------------------------------

#[test]
fn explain_narrative_surfaces_a_rules_reason() {
    let rules = vec![rule_full(
        Some("Work"),
        Some("Legal"),
        AiPolicyMode::Forbidden,
        Some("us"),
        Some("attorney-client privileged correspondence"),
    )];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    let explanation = engine.explain(&PolicyTarget::account("Work").mailbox("Legal"));
    assert!(
        explanation
            .narrative
            .contains("attorney-client privileged correspondence"),
        "narrative: {}",
        explanation.narrative
    );
    assert_eq!(explanation.candidates.len(), 1);
    assert_eq!(explanation.candidates[0].residency.as_deref(), Some("us"));
}

// ---------------------------------------------------------------------------
// Every resolution is logged
// ---------------------------------------------------------------------------

/// A `MakeWriter` that appends everything into a shared buffer — the same
/// pattern `telemetry::tests` uses to assert on captured log output without
/// touching stdout or global subscriber state.
#[derive(Clone, Default)]
struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| io::Error::other("log buffer poisoned"))?;
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for BufWriter {
    type Writer = BufWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn resolve_logs_the_resolution() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = BufWriter(buf.clone());
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_writer(writer),
    );

    let rules = vec![rule(Some("Work"), Some("Legal"), AiPolicyMode::Forbidden)];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    tracing::subscriber::with_default(subscriber, || {
        // `resolve`, not `explain` — the log must fire on the primary
        // entrypoint every AI path actually calls, not only when a caller
        // separately asks for the trace.
        let _ = engine.resolve(&PolicyTarget::account("Work").mailbox("Legal"));
    });

    let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(!captured.is_empty(), "nothing was logged");
    assert!(captured.contains("Work"), "account missing: {captured}");
    assert!(captured.contains("Legal"), "mailbox missing: {captured}");
    assert!(
        captured.contains("forbidden"),
        "resolved mode missing: {captured}"
    );
    assert!(
        captured.contains("ai policy resolved"),
        "event message missing: {captured}"
    );
}

#[test]
fn a_forbidden_resolution_logs_at_info_so_it_survives_the_default_filter() {
    // `telemetry::init` defaults to an `info` EnvFilter; a denial is the
    // security-relevant outcome that must be visible under that default, not
    // only when someone happens to be running with `debug` on.
    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = BufWriter(buf.clone());
    let subscriber = tracing_subscriber::registry()
        .with(tracing::level_filters::LevelFilter::INFO)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_writer(writer),
        );

    let rules = vec![rule(Some("Work"), Some("Legal"), AiPolicyMode::Forbidden)];
    let engine = PolicyEngine::new(rules, AiPolicyMode::Allowed, "unspecified").unwrap();

    tracing::subscriber::with_default(subscriber, || {
        let _ = engine.resolve(&PolicyTarget::account("Work").mailbox("Legal"));
    });

    let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(
        !captured.is_empty(),
        "a forbidden resolution must be visible even with debug-level events filtered out"
    );
}

#[test]
fn an_allowed_resolution_does_not_log_at_info() {
    // The hot-path default stays at `debug`, so it does not survive an
    // `info`-filtered subscriber — only denials are elevated.
    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = BufWriter(buf.clone());
    let subscriber = tracing_subscriber::registry()
        .with(tracing::level_filters::LevelFilter::INFO)
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .flatten_event(true)
                .with_writer(writer),
        );

    let engine = PolicyEngine::new(Vec::new(), AiPolicyMode::Allowed, "unspecified").unwrap();

    tracing::subscriber::with_default(subscriber, || {
        let _ = engine.resolve(&PolicyTarget::account("Work").mailbox("INBOX"));
    });

    let captured = buf.lock().unwrap().clone();
    assert!(
        captured.is_empty(),
        "an allowed resolution should stay at debug and be filtered out here"
    );
}
