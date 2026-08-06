//! [`FeatureVector`]: the per-candidate output of Stage 3 (prd.md, "Stage 3 —
//! Feature Extraction"), and the two things task 65's offline trainer needs
//! from its shape — a stable field order and a lossless round trip.
//!
//! # A typed struct, not a `HashMap<FeatureName, f64>`
//!
//! A map would make "did we compute every feature" a runtime question
//! (`vec.get(FeatureName::Bm25Subject)` returning `None` is indistinguishable
//! from "not computed yet" and "genuinely zero") and would serialize with
//! whatever key order its hasher happens to produce — exactly the
//! byte-identical-replay property this task's acceptance bullet asks for. A
//! plain struct makes every field mandatory at the type level (the compiler
//! rejects a [`FeatureVector`] literal missing one) and `serde_json` — the
//! only JSON backend already in this workspace's `Cargo.toml` — serializes a
//! struct's fields in declaration order, not sorted or hashed. Field
//! declaration order below matches [`crate::features::FeatureName::ALL`]'s
//! order exactly, group by group, so the two never have to be reconciled by
//! hand — [`FeatureVector::as_pairs`] and `vector::tests`' completeness test
//! both depend on that invariant holding.
//!
//! # No field is ever `NaN`
//!
//! `serde_json` cannot represent `NaN`/`±Infinity` at all — serializing one
//! is a hard `Err`, not a lossy `null`. Rather than let a degenerate upstream
//! value (a zero-norm embedding making a cosine similarity `0.0 / 0.0`, a
//! half-life of `0.0` making a decay exponent divide by zero) surface as a
//! serialization failure on the search hot path, every arithmetic path in
//! [`crate::features::extract`] that could produce a non-finite value is
//! routed through [`finite`] first. A production candidate should never
//! actually hit this — it exists so a degenerate input degrades to `0.0`
//! (a deliberately unremarkable ranking value) instead of either panicking
//! or silently breaking the "same inputs, byte-identical vector" contract
//! (`NaN != NaN`, so two structurally-equal extractions could compare unequal
//! if one ever leaked through).
//!
//! # `Option`, not a sentinel, for "not applicable"
//!
//! [`FeatureVector::proximity_min_span`] is `None` rather than `0` when fewer
//! than two of the query's terms are present in a candidate's text (a
//! "window covering all terms" is not a concept that applies) — `0` would be
//! a real, if impossible, span (no two distinct terms can occupy the same
//! token position), so overloading it as "not applicable" would make a
//! consumer that forgets to check unable to tell the two apart. `serde_json`
//! represents `None` as JSON `null`, which is exactly the "no data" reading a
//! future consumer needs. [`FeatureVector::as_pairs`] — the flat numeric form
//! [`crate::features::FeatureName`]'s docs describe task 31/65 wanting — maps
//! `None` to `0.0` there instead, documented at that call site; the typed
//! field itself never lies about it.

use serde::{Deserialize, Serialize};

use super::name::FeatureName;
use crate::retrieve::Source;

/// Clamp a value that must serialize and compare deterministically:
/// non-finite (`NaN`, `±inf`) becomes `0.0`, a well-defined "no signal"
/// value, rather than propagating a corrupt float into a
/// [`FeatureVector`] — see the module docs' "No field is ever `NaN`"
/// section.
#[must_use]
pub(crate) fn finite(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

/// Which field prd.md's Stage 3 `best_match_field` names — the strongest
/// per-field BM25 signal a candidate has, or [`MatchField::None`] when the
/// candidate has no lexical match on any of the four fields at all (a pure
/// dense/fuzzy/entity hit).
///
/// Only the four fields prd.md's own `best_match_field` description names
/// ("subject / from / body / attachment") are represented — `notes` and
/// `summary` are real `bm25()` columns (see [`crate::index::fts`]) but are
/// not one of the categories prd.md's feature table enumerates for this
/// field, so [`crate::features::extract`] never selects them here even
/// though it still folds their weight out of the isolated `bm25()` calls
/// (weight `0`, same as every other column not being isolated for a given
/// call) it does compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchField {
    /// No positive per-field BM25 signal on this candidate.
    None,
    /// Subject-column BM25 was the strongest signal.
    Subject,
    /// Sender-column ("from") BM25 was the strongest signal.
    From,
    /// Body-column BM25 was the strongest signal.
    Body,
    /// Attachment-text-column BM25 was the strongest signal.
    Attachment,
}

/// Serializes/deserializes [`Source`] by its stable lowercase name rather
/// than deriving `Serialize`/`Deserialize` on [`crate::retrieve::Source`]
/// itself.
///
/// `retrieve::Source` is task 28's type, not this task's — `fuse::mod`
/// already made the same call for a different reason (adding `Ord` there to
/// get a total order, rather than touching a type outside its own module;
/// see that module's `source_ordinal` docs) and this module follows the same
/// discipline for the same reason: a shared type gets no `#[derive]` added
/// by a downstream consumer that only needs one specific capability from it,
/// so a future edit to [`Source`] never has to consider what this module
/// coupled to it.
mod source_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::retrieve::Source;

    pub(super) fn serialize<S: Serializer>(
        source: &Source,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        as_str(*source).serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Source, D::Error> {
        let raw = String::deserialize(deserializer)?;
        from_str(&raw).ok_or_else(|| serde::de::Error::custom(format!("unknown source {raw:?}")))
    }

    pub(super) const fn as_str(source: Source) -> &'static str {
        match source {
            Source::Lexical => "lexical",
            Source::Dense => "dense",
            Source::Fuzzy => "fuzzy",
            Source::Entity => "entity",
            Source::Structured => "structured",
            Source::Prefix => "prefix",
            Source::Recency => "recency",
        }
    }

    fn from_str(raw: &str) -> Option<Source> {
        match raw {
            "lexical" => Some(Source::Lexical),
            "dense" => Some(Source::Dense),
            "fuzzy" => Some(Source::Fuzzy),
            "entity" => Some(Source::Entity),
            "structured" => Some(Source::Structured),
            "prefix" => Some(Source::Prefix),
            "recency" => Some(Source::Recency),
            _ => None,
        }
    }

    /// A fixed ordinal for [`Source`], matching prd.md's Stage 1 retriever
    /// table row order — the same order [`fuse::source_ordinal`](crate::fuse)
    /// uses, duplicated rather than imported for the same reason this whole
    /// module avoids touching `retrieve::Source`: a three-line private
    /// ordinal is cheaper to keep in sync by inspection than a cross-module
    /// dependency on another task's private helper.
    pub(super) const fn ordinal(source: Source) -> u8 {
        match source {
            Source::Lexical => 0,
            Source::Dense => 1,
            Source::Fuzzy => 2,
            Source::Entity => 3,
            Source::Structured => 4,
            Source::Prefix => 5,
            Source::Recency => 6,
        }
    }
}

/// The per-candidate feature vector (prd.md, "Stage 3 — Feature Extraction").
///
/// Fields are declared in [`FeatureName::ALL`]'s order, group by group
/// (textual, semantic, fusion, personal, temporal, status, structural,
/// global) — see the module docs for why that ordering is load-bearing
/// rather than cosmetic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeatureVector {
    // -- textual -------------------------------------------------------
    /// Subject-column BM25, field-weighted. `0.0` when the candidate has no
    /// lexical match at all (a pure dense/fuzzy/entity hit).
    pub bm25_subject: f64,
    /// Body-column BM25, field-weighted.
    pub bm25_body: f64,
    /// Sender-column BM25, field-weighted (prd.md's `from`).
    pub bm25_from: f64,
    /// Attachment-text-column BM25, field-weighted.
    pub bm25_attach: f64,
    /// The query's quoted phrase(s) appear verbatim (case-insensitive,
    /// whitespace-normalized) in the subject or body.
    pub exact_phrase_hit: bool,
    /// Fraction, `0.0..=1.0`, of the query's non-negated free-text terms
    /// present as whole tokens anywhere in the candidate's subject/from/to/
    /// cc/body text. `1.0` (vacuously) when the query has no free-text terms
    /// to cover.
    pub term_coverage: f64,
    /// Token width of the tightest window (over the subject+body token
    /// stream) covering every non-negated free-text term at least once.
    /// `None` when the query has fewer than two such terms, or when at least
    /// one of them is entirely absent from the candidate's subject/body — a
    /// "window covering all terms" is undefined in either case, not `0`.
    pub proximity_min_span: Option<u32>,
    /// Which field the strongest positive per-field BM25 signal came from.
    pub best_match_field: MatchField,
    /// Best fuzzy subsequence/trigram score from [`Source::Fuzzy`]'s hit,
    /// `0.0` if no fuzzy hit.
    pub fuzzy_score: f64,

    // -- semantic --------------------------------------------------------
    /// Max chunk cosine similarity from [`Source::Dense`]'s hit, `0.0` if no
    /// dense hit.
    pub cos_max_chunk: f64,
    /// Mean chunk cosine similarity from [`Source::Dense`]'s hit, `0.0` if no
    /// dense hit or the hit carried no mean.
    pub cos_mean_chunk: f64,

    // -- fusion ------------------------------------------------------------
    /// The fused score task 29 computed for this candidate (RRF sum, or the
    /// linear blend, depending on `search.fusion`).
    pub rrf_score: f64,
    /// How many of the seven sources returned this candidate.
    pub num_sources_hit: u32,
    /// Which source contributed the fused score's single largest weighted
    /// term.
    #[serde(with = "source_serde")]
    pub best_source: Source,

    // -- personal ----------------------------------------------------------
    /// Messages exchanged with this sender (saturating), weighted by how
    /// recently. `0.0` when the sender has no `contacts` row.
    pub sender_affinity: f64,
    /// Some message in this candidate's thread carries `\Answered`.
    pub user_replied_thread: bool,
    /// Historical open rate from this sender. Always `0.0` in this build —
    /// see `extract`'s docs: no impression/action log exists until task 64.
    pub prior_opens_from_sender: f64,
    /// Recent traffic in this candidate's thread (saturating message count,
    /// weighted by how recent the thread's last message is). `0.0` when the
    /// candidate has no thread.
    pub thread_activity: f64,

    // -- temporal ------------------------------------------------------
    /// Message age in days from the extraction's reference instant. `None`
    /// when the message has neither `date` nor `internaldate` — deliberately
    /// not `0.0`: a brand-new message (age truly `~0`) and an unscored one
    /// are different facts, and [`FeatureVector::recency_decay`]'s own `0.0`
    /// "unknown" default would otherwise be contradicted by an `age_days` of
    /// `0.0` reading as *maximally* recent to any consumer that does not
    /// separately check for the unknown case.
    pub age_days: Option<f64>,
    /// `exp(-age_days / half_life)`. `0.0` when age is unknown — see
    /// [`FeatureVector::age_days`]'s doc comment for why that is the value
    /// this one field alone uses to mean "unknown" (an unscored message must
    /// not read as maximally recent) rather than needing its own `Option`.
    pub recency_decay: f64,
    /// The message's date falls inside every `before:`/`after:`/`on:`/
    /// `date:` scope the query expressed. `false` when the query expressed
    /// no date scope at all, or the message's date is unknown.
    pub matches_date_intent: bool,

    // -- status --------------------------------------------------------
    /// `\Seen` is absent from this message's flags.
    pub is_unread: bool,
    /// `\Flagged` is present.
    pub is_flagged: bool,
    /// Always `false` in this build — no table backs "pinned" yet. See
    /// `extract`'s docs for the precedent this follows
    /// (`retrieve::lexical`'s `is:pinned` handling).
    pub is_pinned: bool,
    /// AI triage priority, `0.0..=1.0`. Always `0.0` in this build — no
    /// triage table exists until task 48/49.
    pub ai_priority: f64,
    /// A query term matches an applied tag. Always `false` in this build —
    /// no tags table exists until task 55.
    pub has_tag_match: bool,
    /// Inbox-vs-Archive-vs-Spam folder prior, derived from the mailbox name.
    pub folder_prior: f64,

    // -- structural ------------------------------------------------------
    /// The matched text is inside an attachment (`bm25_attach > 0.0`).
    pub has_attachment_match: bool,
    /// This message is its thread's root message.
    pub is_thread_root: bool,
    /// Messages in this message's thread (`0` when it has no thread).
    pub thread_size: u32,
    /// Body length, in characters.
    pub msg_length: u32,

    // -- global ------------------------------------------------------------
    /// Corpus-wide trust in this sender (saturating message-exchange volume,
    /// dampened when [`FeatureVector::is_newsletter`] or
    /// [`FeatureVector::is_automated`] is set). `0.0` when the sender has no
    /// `contacts` row.
    pub sender_reputation: f64,
    /// Heuristically detected bulk/marketing mail (address/display-name
    /// keyword match — see `extract`'s docs).
    pub is_newsletter: bool,
    /// Heuristically detected transactional/system mail.
    pub is_automated: bool,
}

impl FeatureVector {
    /// Every feature as a `(name, value)` pair, in [`FeatureName::ALL`]'s
    /// order — the flat numeric form task 31's linear scorer dot-products
    /// against a name-keyed weight table, and task 65's trainer/GBDT
    /// consumes directly.
    ///
    /// A `bool` becomes `1.0`/`0.0`; [`FeatureVector::proximity_min_span`]'s
    /// `None` becomes `0.0` (a real span is always `>= 1`, so this cannot be
    /// confused with a genuine window — see the module docs for why the
    /// typed field itself stays `Option` rather than pre-collapsing this
    /// here); [`MatchField`]/[`Source`] become a fixed ordinal — meaningless
    /// to a linear model's dot product (which is exactly why
    /// [`crate::config::RankWeights`] assigns no weight to `best_match_field`
    /// or `best_source`; an absent weight contributes `0.0` regardless of
    /// this ordinal's value) but a real, useful split feature for task 65's
    /// tree-based model.
    #[must_use]
    pub fn as_pairs(&self) -> [(FeatureName, f64); 34] {
        [
            (FeatureName::Bm25Subject, self.bm25_subject),
            (FeatureName::Bm25Body, self.bm25_body),
            (FeatureName::Bm25From, self.bm25_from),
            (FeatureName::Bm25Attach, self.bm25_attach),
            (FeatureName::ExactPhraseHit, bool_f64(self.exact_phrase_hit)),
            (FeatureName::TermCoverage, self.term_coverage),
            (
                FeatureName::ProximityMinSpan,
                self.proximity_min_span.map_or(0.0, f64::from),
            ),
            (
                FeatureName::BestMatchField,
                match_field_ordinal(self.best_match_field),
            ),
            (FeatureName::FuzzyScore, self.fuzzy_score),
            (FeatureName::CosMaxChunk, self.cos_max_chunk),
            (FeatureName::CosMeanChunk, self.cos_mean_chunk),
            (FeatureName::RrfScore, self.rrf_score),
            (FeatureName::NumSourcesHit, f64::from(self.num_sources_hit)),
            (
                FeatureName::BestSource,
                f64::from(source_serde::ordinal(self.best_source)),
            ),
            (FeatureName::SenderAffinity, self.sender_affinity),
            (
                FeatureName::UserRepliedThread,
                bool_f64(self.user_replied_thread),
            ),
            (
                FeatureName::PriorOpensFromSender,
                self.prior_opens_from_sender,
            ),
            (FeatureName::ThreadActivity, self.thread_activity),
            // `None` (unknown age) becomes `0.0` here, same sentinel-free
            // convention as `proximity_min_span` above: a real age is always
            // `>= 0.0`, so a linear/GBDT consumer of this flat form cannot
            // mistake "unknown" for "extremely old" or "brand new" from this
            // value alone — it can, however, cross-reference
            // `recency_decay`, which uses `0.0` to mean exactly "unknown"
            // (see both fields' doc comments on the typed struct).
            (FeatureName::AgeDays, self.age_days.unwrap_or(0.0)),
            (FeatureName::RecencyDecay, self.recency_decay),
            (
                FeatureName::MatchesDateIntent,
                bool_f64(self.matches_date_intent),
            ),
            (FeatureName::IsUnread, bool_f64(self.is_unread)),
            (FeatureName::IsFlagged, bool_f64(self.is_flagged)),
            (FeatureName::IsPinned, bool_f64(self.is_pinned)),
            (FeatureName::AiPriority, self.ai_priority),
            (FeatureName::HasTagMatch, bool_f64(self.has_tag_match)),
            (FeatureName::FolderPrior, self.folder_prior),
            (
                FeatureName::HasAttachmentMatch,
                bool_f64(self.has_attachment_match),
            ),
            (FeatureName::IsThreadRoot, bool_f64(self.is_thread_root)),
            (FeatureName::ThreadSize, f64::from(self.thread_size)),
            (FeatureName::MsgLength, f64::from(self.msg_length)),
            (FeatureName::SenderReputation, self.sender_reputation),
            (FeatureName::IsNewsletter, bool_f64(self.is_newsletter)),
            (FeatureName::IsAutomated, bool_f64(self.is_automated)),
        ]
    }
}

fn bool_f64(value: bool) -> f64 {
    if value {
        1.0
    } else {
        0.0
    }
}

/// `MatchField`'s ordinal for [`FeatureVector::as_pairs`], in the same order
/// [`MatchField`] itself is declared.
fn match_field_ordinal(field: MatchField) -> f64 {
    match field {
        MatchField::None => 0.0,
        MatchField::Subject => 1.0,
        MatchField::From => 2.0,
        MatchField::Body => 3.0,
        MatchField::Attachment => 4.0,
    }
}

#[cfg(test)]
mod tests;
