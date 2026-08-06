//! What task 31/33/65 need from [`FeatureName`] to stay safe: every variant
//! reachable from [`FeatureName::ALL`] exactly once (so a `for name in
//! FeatureName::ALL` loop — which is exactly how a weights lookup or an
//! `Explain` response is built — never silently skips or double-counts a
//! feature), and a name that survives a round trip through its own string
//! form (the shape a TOML weights file or a logged training row actually
//! stores).

use std::collections::BTreeSet;

use super::*;

/// prd.md's Stage 3 table lists 34 distinct feature names across the
/// textual/semantic/fusion/personal/temporal/status/structural/global
/// groups (see this crate's `features` module docs for the full accounting).
/// Pinning the count here means a future edit that adds or removes a
/// [`FeatureName`] variant without updating [`FeatureName::ALL`] fails loudly
/// instead of silently shipping an incomplete vector.
#[test]
fn all_has_the_documented_feature_count() {
    assert_eq!(FeatureName::ALL.len(), 34);
}

/// Every variant is reachable from `ALL` and none is duplicated — the
/// completeness property [`crate::features::FeatureVector::as_pairs`]
/// depends on to cover every field exactly once.
#[test]
fn every_variant_is_in_all_exactly_once() {
    let all_variants = [
        FeatureName::Bm25Subject,
        FeatureName::Bm25Body,
        FeatureName::Bm25From,
        FeatureName::Bm25Attach,
        FeatureName::ExactPhraseHit,
        FeatureName::TermCoverage,
        FeatureName::ProximityMinSpan,
        FeatureName::BestMatchField,
        FeatureName::FuzzyScore,
        FeatureName::CosMaxChunk,
        FeatureName::CosMeanChunk,
        FeatureName::RrfScore,
        FeatureName::NumSourcesHit,
        FeatureName::BestSource,
        FeatureName::SenderAffinity,
        FeatureName::UserRepliedThread,
        FeatureName::PriorOpensFromSender,
        FeatureName::ThreadActivity,
        FeatureName::AgeDays,
        FeatureName::RecencyDecay,
        FeatureName::MatchesDateIntent,
        FeatureName::IsUnread,
        FeatureName::IsFlagged,
        FeatureName::IsPinned,
        FeatureName::AiPriority,
        FeatureName::HasTagMatch,
        FeatureName::FolderPrior,
        FeatureName::HasAttachmentMatch,
        FeatureName::IsThreadRoot,
        FeatureName::ThreadSize,
        FeatureName::MsgLength,
        FeatureName::SenderReputation,
        FeatureName::IsNewsletter,
        FeatureName::IsAutomated,
    ];
    let want: BTreeSet<&str> = all_variants.iter().map(|v| v.as_str()).collect();
    let got: BTreeSet<&str> = FeatureName::ALL.iter().map(|v| v.as_str()).collect();
    assert_eq!(
        want.len(),
        all_variants.len(),
        "the hand-written list itself has a duplicate"
    );
    assert_eq!(
        got, want,
        "FeatureName::ALL drifted from the enum's own variants"
    );
}

/// Every name is a distinct, non-empty, `snake_case`-looking string — the
/// shape a TOML `[search.rank_weights]` table key or a JSON training-row
/// field name needs.
#[test]
fn every_name_is_unique_and_snake_case() {
    let mut seen = BTreeSet::new();
    for name in FeatureName::ALL {
        let s = name.as_str();
        assert!(!s.is_empty());
        assert!(
            s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{s:?} is not snake_case"
        );
        assert!(seen.insert(s), "{s:?} is not unique");
    }
}

/// `Display` and `as_str` must agree — `Explain`/logging code reaches for
/// whichever is more convenient and both must produce the same key.
#[test]
fn display_matches_as_str() {
    for name in FeatureName::ALL {
        assert_eq!(name.to_string(), name.as_str());
    }
}

/// Every group named in the module docs is actually assigned to at least one
/// feature — a group nothing maps to would mean the eight-group accounting
/// this module claims is wrong.
#[test]
fn every_group_has_at_least_one_feature() {
    let groups = [
        FeatureGroup::Textual,
        FeatureGroup::Semantic,
        FeatureGroup::Fusion,
        FeatureGroup::Personal,
        FeatureGroup::Temporal,
        FeatureGroup::Status,
        FeatureGroup::Structural,
        FeatureGroup::Global,
    ];
    for group in groups {
        assert!(
            FeatureName::ALL.iter().any(|n| n.group() == group),
            "{group:?} has no feature assigned to it"
        );
    }
}
