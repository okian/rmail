//! The trait contract, the invariants a vector carries, and the degradation
//! rules that decide which backend a configuration actually gets.

use super::*;
use crate::config::{LocalEmbedConfig, VoyageConfig};

// ---------------------------------------------------------------------------
// Embedding
// ---------------------------------------------------------------------------

#[test]
fn an_embedding_is_unit_length_however_it_was_built() {
    // The invariant every consumer relies on: cosine is a dot product, so if a
    // single vector escapes unnormalized every comparison it takes part in is
    // silently scaled.
    // The last four are the ones that matter: in `f32` the sum of squares
    // overflows above ~1.8e19 per component and underflows below ~1e-22, and
    // both failures are silent — an unnormalized vector escapes at one end and
    // a perfectly good direction becomes all zeros at the other. Stopping at
    // 1e-8, as an earlier version of this test did, is fifteen orders of
    // magnitude short of the boundary.
    for raw in [
        vec![3.0, 4.0],
        vec![1.0; 384],
        vec![-2.0, 0.0, 0.0],
        vec![1e-8, 1e-8],
        vec![1e-23, 1e-23],
        vec![f32::from_bits(1); 2],
        vec![1e30, 1e-30],
        vec![f32::MAX, 1.0],
    ] {
        let e = Embedding::new(raw.clone());
        let norm = e.as_slice().iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "norm {norm} for {raw:?} should be 1"
        );
    }
}

#[test]
fn a_zero_vector_stays_zero_rather_than_becoming_nan() {
    // An empty or entirely out-of-vocabulary input produces this. Dividing by a
    // norm of zero fills the vector with NaN, and a NaN makes every comparison
    // it appears in false rather than merely wrong — including the ones that
    // decide ranking.
    let e = Embedding::new(vec![0.0; 8]);
    assert!(e.as_slice().iter().all(|v| *v == 0.0));
    assert_eq!(e.cosine(&Embedding::new(vec![1.0; 8])), 0.0);
}

#[test]
fn a_model_that_returned_nonsense_cannot_poison_a_comparison() {
    let e = Embedding::new(vec![f32::NAN, f32::INFINITY, 3.0, 4.0]);
    assert!(
        e.as_slice().iter().all(|v| v.is_finite()),
        "got {:?}",
        e.as_slice()
    );
    let other = Embedding::new(vec![0.0, 0.0, 3.0, 4.0]);
    assert!((e.cosine(&other) - 1.0).abs() < 1e-4);
}

#[test]
fn cosine_ranges_over_minus_one_to_one() {
    let a = Embedding::new(vec![1.0, 0.0]);
    let b = Embedding::new(vec![0.0, 1.0]);
    let opposite = Embedding::new(vec![-1.0, 0.0]);
    assert!((a.cosine(&a) - 1.0).abs() < 1e-6);
    assert!(a.cosine(&b).abs() < 1e-6);
    assert!((a.cosine(&opposite) + 1.0).abs() < 1e-6);
}

#[test]
fn vectors_from_different_models_do_not_compare() {
    // Mixing two models' vectors in one index is a configuration mistake. A
    // plausible-looking score computed over a shared prefix would hide it for
    // as long as the index lived.
    let small = Embedding::new(vec![1.0, 0.0, 0.0]);
    let large = Embedding::new(vec![1.0, 0.0, 0.0, 0.0]);
    assert_eq!(small.cosine(&large), 0.0);
}

#[test]
fn a_vector_survives_a_round_trip_through_storage() {
    let original = Embedding::new((0..384).map(|n| n as f32 - 190.0).collect());
    let restored = Embedding::from_bytes(&original.to_bytes(), 384).unwrap();
    assert_eq!(restored.dim(), 384);
    for (a, b) in original.as_slice().iter().zip(restored.as_slice()) {
        assert!((a - b).abs() < 1e-6);
    }
}

#[test]
fn a_truncated_blob_is_an_error_not_a_shorter_vector() {
    // Reading corruption as a shorter vector turns a detectable fault into a
    // quietly wrong ranking that nothing ever reports: a dimension mismatch
    // scores zero against everything, and since real cosines from these models
    // are comfortably positive, zero sorts last rather than being noticed.
    let bytes = Embedding::new(vec![1.0, 2.0, 3.0]).to_bytes();
    for (blob, why) in [
        (&bytes[..bytes.len() - 1], "not a whole number of f32s"),
        (&bytes[..4], "the right shape but the wrong length"),
        (
            &bytes[..0],
            "empty, which is a plausible thing for a BLOB to be",
        ),
    ] {
        let err = Embedding::from_bytes(blob, 3).unwrap_err();
        assert_eq!(err.reason(), crate::ErrorReason::InvalidArgument, "{why}");
    }
}

#[test]
fn a_stored_vector_reads_back_bit_identical() {
    // So that "has this cached vector changed?" is answerable by comparison.
    // Re-normalizing unconditionally on read moves every component by an ulp or
    // two, which is harmless for scoring and fatal for that question.
    let original = Embedding::new((0..384).map(|n| (n as f32).sin()).collect());
    let bytes = original.to_bytes();
    let restored = Embedding::from_bytes(&bytes, 384).unwrap();
    assert_eq!(restored, original);
    assert_eq!(restored.to_bytes(), bytes);
}

#[test]
fn a_debug_line_does_not_carry_the_vector() {
    // An embedding is partially invertible back to its source text, so a
    // `{:?}` on any struct holding one would put message content in the logs.
    let rendered = format!("{:?}", Embedding::new(vec![0.5; 384]));
    assert_eq!(rendered, "Embedding(384 dims)");
    assert!(!rendered.contains("0.5"));
}

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

#[test]
fn truncation_lands_on_a_character_boundary() {
    // A fixed byte limit meeting arbitrary mail. Slicing a UTF-8 string at an
    // arbitrary offset panics, and non-ASCII long mail is ordinary, not exotic.
    let text = "é".repeat(MAX_INPUT_BYTES);
    let cut = truncate(&text);
    assert!(cut.len() <= MAX_INPUT_BYTES);
    assert!(text.starts_with(cut));
    assert!(!cut.is_empty());
}

#[test]
fn short_text_is_returned_whole() {
    assert_eq!(truncate("hello"), "hello");
}

#[test]
fn truncation_always_cuts_the_same_prefix() {
    // The content-hash cache re-embeds only when content changes; a truncation
    // that varied would make an unchanged message produce a different vector
    // and quietly invalidate the whole cache.
    let text = "word ".repeat(MAX_INPUT_BYTES);
    assert_eq!(truncate(&text), truncate(&text.clone()));
}

// ---------------------------------------------------------------------------
// The hashing fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_fallback_is_deterministic_across_calls() {
    // Vectors are cached against a content hash and read back after restarts
    // and upgrades. A backend whose output drifted would make every cached
    // vector wrong rather than merely stale.
    let e = hash::HashEmbedder::new(384);
    let texts = vec!["the quarterly invoice".to_owned()];
    let first = e.embed(&texts).await.unwrap();
    let second = e.embed(&texts).await.unwrap();
    assert_eq!(first, second);
}

#[tokio::test]
async fn the_fallback_returns_one_vector_per_input_in_order() {
    let e = hash::HashEmbedder::new(128);
    let texts: Vec<String> = (0..5).map(|n| format!("message {n}")).collect();
    let vectors = e.embed(&texts).await.unwrap();

    assert_eq!(vectors.len(), 5);
    for (n, vector) in vectors.iter().enumerate() {
        let alone = e.embed(&[texts[n].clone()]).await.unwrap();
        assert_eq!(
            vector, &alone[0],
            "vector {n} must belong to input {n} whether batched or not"
        );
    }
}

#[tokio::test]
async fn the_fallback_recovers_lexical_overlap() {
    // It is not semantic and does not claim to be, but if it cannot even tell
    // overlapping text from unrelated text it is noise rather than a fallback.
    let e = hash::HashEmbedder::new(384);
    let vectors = e
        .embed(&[
            "the invoice for the quarterly hosting bill".to_owned(),
            "the invoice for the quarterly hosting charge".to_owned(),
            "lunch plans for saturday afternoon".to_owned(),
        ])
        .await
        .unwrap();

    let near = vectors[0].cosine(&vectors[1]);
    let far = vectors[0].cosine(&vectors[2]);
    assert!(
        near > far + 0.3,
        "overlapping text {near} should beat unrelated text {far} clearly"
    );
}

#[tokio::test]
async fn the_fallback_tolerates_a_typo() {
    // The character 3-grams are what buy this; whole-word hashing alone would
    // make a single transposed letter as distant as a different sentence.
    let e = hash::HashEmbedder::new(384);
    let vectors = e
        .embed(&[
            "quarterly hosting invoice".to_owned(),
            "quarterly hostng invoice".to_owned(),
            "saturday lunch plans".to_owned(),
        ])
        .await
        .unwrap();
    assert!(vectors[0].cosine(&vectors[1]) > vectors[0].cosine(&vectors[2]));
}

#[tokio::test]
async fn the_fallback_embeds_empty_text_without_failing() {
    // A scanned PDF with no text layer reaches here as an empty string.
    let e = hash::HashEmbedder::new(128);
    let vectors = e.embed(&["".to_owned()]).await.unwrap();
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].dim(), 128);
}

#[test]
fn the_fallback_will_not_produce_a_uselessly_small_vector() {
    // Below the floor, collisions dominate and the vector says more about the
    // hash than the text. Clamped rather than rejected: `dim` comes from a
    // config file, and refusing to start is worse than correcting it.
    assert_eq!(hash::HashEmbedder::new(0).dim(), 64);
    assert_eq!(hash::HashEmbedder::new(1).dim(), 64);
    assert_eq!(hash::HashEmbedder::new(384).dim(), 384);
}

#[tokio::test]
async fn the_fallback_says_what_it_is() {
    // Stored in the vector's `model` column. A row embedded by the fallback
    // must never be silently compared against one from a real model.
    let e = hash::HashEmbedder::new(384);
    assert_eq!(e.model(), "hash-fallback");
    e.warm().await.unwrap();
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

#[test]
fn the_default_configuration_gives_a_local_embedder() {
    let config = IndexSemanticConfig::default();
    let embedder = build(&config).unwrap();
    assert_eq!(embedder.dim(), 384);
    #[cfg(feature = "onnx")]
    assert_eq!(embedder.model(), "bge-small-en-v1.5");
    #[cfg(not(feature = "onnx"))]
    assert_eq!(embedder.model(), "hash-fallback");
}

#[test]
fn building_an_embedder_does_no_io() {
    // Constructing one happens in every daemon and every test. If it loaded a
    // model, no test could run without several hundred megabytes on disk.
    let started = std::time::Instant::now();
    let _ = build(&IndexSemanticConfig::default()).unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(200),
        "took {:?}",
        started.elapsed()
    );
}

#[test]
fn disabling_the_provider_still_yields_an_embedder() {
    // Graceful degradation is a stated requirement: the retrieval pipeline
    // below this has one code path, not a populated one and an empty one.
    let config = IndexSemanticConfig {
        provider: SemanticProvider::None,
        local: LocalEmbedConfig {
            dim: 256,
            ..LocalEmbedConfig::default()
        },
        ..IndexSemanticConfig::default()
    };
    let embedder = build(&config).unwrap();
    assert_eq!(embedder.model(), "hash-fallback");
    assert_eq!(embedder.dim(), 256);
}

#[test]
fn voyage_without_a_key_command_fails_at_build_not_at_query() {
    // A misconfigured daemon should fail while somebody is watching it start,
    // not on the first user query hours later.
    let config = IndexSemanticConfig {
        provider: SemanticProvider::Voyage,
        voyage: VoyageConfig {
            api_key_command: "   ".to_owned(),
            ..VoyageConfig::default()
        },
        ..IndexSemanticConfig::default()
    };
    let err = build(&config).unwrap_err();
    assert_eq!(err.reason(), crate::ErrorReason::FailedPrecondition);
}
