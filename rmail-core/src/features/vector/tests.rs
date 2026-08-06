//! What task 64/65 need from [`FeatureVector`] to actually work: a
//! serialize→deserialize round trip that reproduces the value exactly
//! (including `None` and a would-be-`NaN` sanitized away before it ever
//! reaches the struct), and a `(name, value)` flattening that covers every
//! [`FeatureName`] exactly once, in order.

use super::*;
use crate::features::name::FeatureName;

/// A vector with a distinct, recognizable value in every field — the
/// difference between "the round trip preserved order" and "the round trip
/// happened to still look right because two fields share a default".
fn sample() -> FeatureVector {
    FeatureVector {
        bm25_subject: 8.5,
        bm25_body: 1.25,
        bm25_from: 4.0,
        bm25_attach: 0.0,
        exact_phrase_hit: true,
        term_coverage: 0.75,
        proximity_min_span: Some(3),
        best_match_field: MatchField::Subject,
        fuzzy_score: 0.42,
        cos_max_chunk: 0.91,
        cos_mean_chunk: 0.63,
        rrf_score: 0.031_5,
        num_sources_hit: 3,
        best_source: Source::Dense,
        sender_affinity: 0.55,
        user_replied_thread: true,
        prior_opens_from_sender: 0.0,
        thread_activity: 0.2,
        age_days: Some(12.5),
        recency_decay: 0.687,
        matches_date_intent: false,
        is_unread: true,
        is_flagged: false,
        is_pinned: false,
        ai_priority: 0.0,
        has_tag_match: false,
        folder_prior: 1.0,
        has_attachment_match: false,
        is_thread_root: true,
        thread_size: 4,
        msg_length: 812,
        sender_reputation: 0.4,
        is_newsletter: false,
        is_automated: false,
    }
}

// ---------------------------------------------------------------------------
// Serialization round trip
// ---------------------------------------------------------------------------

#[test]
fn round_trips_through_json_unchanged() {
    let original = sample();
    let json = serde_json::to_string(&original).expect("serialize");
    let restored: FeatureVector = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(original, restored);
}

/// Serializing the same vector twice must produce byte-identical output —
/// task 64 logs this JSON as an impression and task 65 replays it; anything
/// less than byte-identical (a `HashMap`-backed field whose key order varies
/// run to run, for example) would make replay non-reproducible even though
/// this module has no such field today.
#[test]
fn serializing_twice_is_byte_identical() {
    let v = sample();
    let a = serde_json::to_string(&v).expect("serialize");
    let b = serde_json::to_string(&v).expect("serialize");
    assert_eq!(a, b);
}

/// Field order in the JSON output matches declaration order (`serde_json`'s
/// default for a struct) — a positional/order-sensitive replay tool can rely
/// on it without parsing into a map first.
#[test]
fn json_field_order_matches_declaration_order() {
    let json = serde_json::to_string(&sample()).expect("serialize");
    let subject_at = json.find("\"bm25_subject\"").expect("bm25_subject present");
    let body_at = json.find("\"bm25_body\"").expect("bm25_body present");
    let automated_at = json.find("\"is_automated\"").expect("is_automated present");
    assert!(subject_at < body_at, "bm25_subject must precede bm25_body");
    assert!(
        body_at < automated_at,
        "an early textual field must precede the last global field"
    );
}

/// `None` serializes as JSON `null` and survives the round trip as `None` —
/// not `0`, not a missing key a lenient deserializer would default away
/// (this struct has no `#[serde(default)]`, so a dropped key would be a hard
/// deserialize error, not a silent `0`).
#[test]
fn proximity_min_span_none_round_trips_as_null() {
    let mut v = sample();
    v.proximity_min_span = None;
    let json = serde_json::to_string(&v).expect("serialize");
    assert!(
        json.contains("\"proximity_min_span\":null"),
        "expected a literal null, got: {json}"
    );
    let restored: FeatureVector = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.proximity_min_span, None);
}

#[test]
fn proximity_min_span_some_round_trips() {
    let mut v = sample();
    v.proximity_min_span = Some(7);
    let json = serde_json::to_string(&v).expect("serialize");
    let restored: FeatureVector = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored.proximity_min_span, Some(7));
}

/// [`MatchField`] and [`Source`] both round-trip through their stable string
/// form, not a numeric discriminant that would silently renumber if a
/// variant were ever reordered.
#[test]
fn categorical_fields_round_trip_by_name() {
    for field in [
        MatchField::None,
        MatchField::Subject,
        MatchField::From,
        MatchField::Body,
        MatchField::Attachment,
    ] {
        let mut v = sample();
        v.best_match_field = field;
        let json = serde_json::to_string(&v).expect("serialize");
        let restored: FeatureVector = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.best_match_field, field);
    }
    for source in [
        Source::Lexical,
        Source::Dense,
        Source::Fuzzy,
        Source::Entity,
        Source::Structured,
        Source::Prefix,
        Source::Recency,
    ] {
        let mut v = sample();
        v.best_source = source;
        let json = serde_json::to_string(&v).expect("serialize");
        let restored: FeatureVector = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.best_source, source);
        assert!(json.contains(&format!(
            "\"best_source\":\"{}\"",
            source_serde::as_str(source)
        )));
    }
}

// ---------------------------------------------------------------------------
// NaN handling
// ---------------------------------------------------------------------------

/// `finite` is what stands between a degenerate upstream computation (a
/// zero-norm cosine, a zero half-life) and a [`FeatureVector`] that cannot
/// serialize at all — see the module docs.
#[test]
fn finite_sanitizes_nan_and_infinities_to_zero() {
    assert_eq!(finite(f64::NAN), 0.0);
    assert_eq!(finite(f64::INFINITY), 0.0);
    assert_eq!(finite(f64::NEG_INFINITY), 0.0);
    // A runtime (not constant-folded) zero division — the shape a genuinely
    // degenerate computation (a zero-norm cosine) would actually produce,
    // rather than the `f64::NAN` literal above.
    let zero = std::hint::black_box(0.0_f64);
    assert_eq!(finite(zero / zero), 0.0);
}

#[test]
fn finite_passes_ordinary_values_through() {
    assert_eq!(finite(0.0), 0.0);
    assert_eq!(finite(-3.5), -3.5);
    assert_eq!(finite(42.125), 42.125);
}

/// A [`FeatureVector`] built from a `finite`-sanitized degenerate input still
/// serializes — the concrete failure `finite` exists to prevent turning into
/// a hard `Err` on the search hot path.
#[test]
fn a_vector_built_from_sanitized_nan_still_serializes() {
    let mut v = sample();
    let zero = std::hint::black_box(0.0_f64);
    v.cos_max_chunk = finite(zero / zero);
    v.recency_decay = finite(f64::INFINITY);
    assert_eq!(v.cos_max_chunk, 0.0);
    assert_eq!(v.recency_decay, 0.0);
    let json = serde_json::to_string(&v).expect("a sanitized vector must always serialize");
    let restored: FeatureVector = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, v);
}

// ---------------------------------------------------------------------------
// as_pairs / completeness
// ---------------------------------------------------------------------------

/// Every [`FeatureName`] appears in [`FeatureVector::as_pairs`] exactly once,
/// in [`FeatureName::ALL`]'s own order — the "vector completeness" property
/// this task's `verify` line names directly.
#[test]
fn as_pairs_covers_every_feature_name_in_order() {
    let pairs = sample().as_pairs();
    let names: Vec<FeatureName> = pairs.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, FeatureName::ALL.to_vec());
}

/// Spot-check a handful of conversions `as_pairs` performs so a future edit
/// to the bool/Option/categorical mapping does not silently drift.
#[test]
fn as_pairs_converts_bool_option_and_categorical_fields() {
    let mut v = sample();
    v.exact_phrase_hit = true;
    v.proximity_min_span = None;
    v.best_match_field = MatchField::Body;
    v.best_source = Source::Entity;
    let pairs = v.as_pairs();
    let get = |name: FeatureName| pairs.iter().find(|(n, _)| *n == name).unwrap().1;

    assert_eq!(get(FeatureName::ExactPhraseHit), 1.0);
    assert_eq!(
        get(FeatureName::ProximityMinSpan),
        0.0,
        "None must not be confused with a real span"
    );
    assert_eq!(
        get(FeatureName::BestMatchField),
        match_field_ordinal(MatchField::Body)
    );
    assert_eq!(
        get(FeatureName::BestSource),
        f64::from(source_serde::ordinal(Source::Entity))
    );

    v.exact_phrase_hit = false;
    let pairs = v.as_pairs();
    let get = |name: FeatureName| pairs.iter().find(|(n, _)| *n == name).unwrap().1;
    assert_eq!(get(FeatureName::ExactPhraseHit), 0.0);
}

/// `as_pairs` never produces a non-finite value given a well-formed
/// (finite-only) [`FeatureVector`] — every field is either already `f64`
/// (guaranteed finite by construction upstream) or a bounded conversion
/// (`bool`, `Option<u32>`, an ordinal) that cannot itself introduce `NaN`.
#[test]
fn as_pairs_values_are_always_finite() {
    for (_, value) in sample().as_pairs() {
        assert!(value.is_finite());
    }
}
