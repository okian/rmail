//! Stable feature identity: every name [`extract::FeatureExtractor`](super::extract::FeatureExtractor)
//! can produce, spelled once, here.
//!
//! # Why an enum, not a string, and why a string too
//!
//! prd.md's Stage 3 table names each feature and never renumbers them across
//! releases the way a positional `Vec<f64>` index silently would (add one
//! feature in the middle of the vector and every downstream consumer keyed by
//! index reads a stale meaning). Task 33's `Explain` reports "which feature
//! contributed what" back to a human, and task 65's offline trainer hot-swaps
//! a model whose weights are keyed by feature — both need a name that
//! survives a struct-field reorder or a new feature landing between two
//! existing ones. [`FeatureName`] is that name: a closed, exhaustively
//! matched enum for compile-time safety inside this crate, with
//! [`FeatureName::as_str`] giving the exact prd.md-table string every
//! external consumer (a TOML weights file, a logged training row, an
//! `Explain` response) actually keys on.
//!
//! # `fusion` is a real fourth group prd.md's own table introduces
//!
//! prd.md's Stage 3 intro paragraph names seven groups ("textual-match,
//! semantic, behavioral/personal, temporal, status, structural, and
//! global-prior"), but the table two paragraphs later gives `rrf_score`,
//! `num_sources_hit`, and `best_source` a `Group` column value of `fusion` —
//! not `textual`, not any of the seven the prose lists. This module follows
//! the table (the actual feature-by-feature spec) over the prose summary
//! that undercounts it by one, and names the eighth group [`FeatureGroup::Fusion`]
//! rather than folding those three features into `Textual` where the prose
//! would imply they belong — flattening the two elsewhere in this codebase's
//! house style would go the other way (see, e.g., `fuse::mod`'s own
//! discrepancy notes), and there is nothing to be gained by pretending the
//! discrepancy is not there.

use std::fmt;

/// One of the seven groups prd.md's Stage 3 prose names, plus the eighth
/// (`fusion`) its own feature table adds — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeatureGroup {
    /// Exact/BM25/fuzzy textual match evidence.
    Textual,
    /// Dense-vector cosine similarity.
    Semantic,
    /// How the retrievers agreed with each other (task 29's fusion output).
    Fusion,
    /// This user's own history with the sender/thread.
    Personal,
    /// Message age and date-scope match.
    Temporal,
    /// Flags, tags, AI triage priority, folder.
    Status,
    /// Message/thread shape.
    Structural,
    /// Corpus-wide, query-independent priors.
    Global,
}

/// Every feature [`crate::features::FeatureVector`] carries, named exactly as
/// prd.md's Stage 3 table names it.
///
/// Declared in the same order [`crate::features::FeatureVector`]'s fields
/// are declared, group by group — [`FeatureName::ALL`] and
/// [`crate::features::FeatureVector::as_pairs`] both rely on that ordering
/// staying in lock-step so a `(FeatureName, f64)` pair always names the field
/// it actually came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FeatureName {
    /// Subject-column BM25, field-weighted.
    Bm25Subject,
    /// Body-column BM25, field-weighted.
    Bm25Body,
    /// Sender-column BM25, field-weighted (prd.md calls the column `from`).
    Bm25From,
    /// Attachment-text-column BM25, field-weighted.
    Bm25Attach,
    /// The query's quoted phrase(s) appear verbatim in the subject or body.
    ExactPhraseHit,
    /// Fraction of the query's free-text terms present anywhere in the
    /// candidate's addressable text.
    TermCoverage,
    /// Token width of the tightest window covering every query term, when
    /// every term is present; absent otherwise.
    ProximityMinSpan,
    /// Which field the strongest per-field BM25 signal came from.
    BestMatchField,
    /// Best fuzzy subsequence/trigram score.
    FuzzyScore,
    /// Max chunk cosine similarity (dense retrieval).
    CosMaxChunk,
    /// Mean chunk cosine similarity (dense retrieval).
    CosMeanChunk,
    /// The fused RRF (or linear-blend) score task 29 produced.
    RrfScore,
    /// How many of the seven sources returned this candidate.
    NumSourcesHit,
    /// Which source contributed the fused score's single largest term.
    BestSource,
    /// Messages exchanged with this sender, weighted by recency.
    SenderAffinity,
    /// The user has replied somewhere in this message's thread.
    UserRepliedThread,
    /// Historical open rate from this sender (task 64/65 feedback loop).
    PriorOpensFromSender,
    /// Recent traffic in this message's thread.
    ThreadActivity,
    /// Message age, in days, from the reference instant.
    AgeDays,
    /// `exp(-age_days / half_life)`.
    RecencyDecay,
    /// The message's date falls inside every date scope the query expressed.
    MatchesDateIntent,
    /// `\Seen` is absent.
    IsUnread,
    /// `\Flagged` is present.
    IsFlagged,
    /// Pinned (no backing data in this build — see `extract`'s docs).
    IsPinned,
    /// AI triage priority, `0.0..=1.0` (no backing data in this build).
    AiPriority,
    /// A query term matches an applied tag (no tags table in this build).
    HasTagMatch,
    /// Inbox-vs-Archive-vs-Spam folder prior.
    FolderPrior,
    /// The matched text is inside an attachment.
    HasAttachmentMatch,
    /// This message is its thread's root.
    IsThreadRoot,
    /// Messages in this message's thread.
    ThreadSize,
    /// Body length, in characters.
    MsgLength,
    /// Corpus-wide trust in this sender, dampened for detected bulk/automated
    /// senders.
    SenderReputation,
    /// Heuristically detected bulk/marketing mail.
    IsNewsletter,
    /// Heuristically detected transactional/system mail.
    IsAutomated,
}

impl FeatureName {
    /// Every feature, in [`crate::features::FeatureVector`]'s declaration
    /// order. Hand-rolled rather than derived (no `strum` in this workspace —
    /// see `.claude/BUILD_BRIEF.md`'s "hand-rolled" test-helper convention,
    /// which applies equally to a small fixed enumeration like this one).
    ///
    /// Two independent guards keep this from silently drifting behind the
    /// enum, neither of them perfect alone: [`FeatureName::as_str`] and
    /// [`FeatureName::group`] are themselves exhaustive matches with no
    /// wildcard arm, so a variant added to this enum without a matching arm
    /// added to *those* fails to **compile** — this array is not what
    /// enforces that half. What compiling does not catch is a variant added
    /// (and handled in `as_str`/`group`) but never added *here*: no `strum`-
    /// style reflection exists in this workspace to enumerate a plain enum's
    /// variants without listing them, so `name::tests::every_variant_is_in_all_exactly_once`'s
    /// hand-written comparison list is the only check for that half, and it
    /// is only as good as remembering to update it alongside this array —
    /// the same manual-sync risk `ALL` itself has, not a stronger guarantee
    /// than it.
    pub const ALL: [FeatureName; 34] = [
        Self::Bm25Subject,
        Self::Bm25Body,
        Self::Bm25From,
        Self::Bm25Attach,
        Self::ExactPhraseHit,
        Self::TermCoverage,
        Self::ProximityMinSpan,
        Self::BestMatchField,
        Self::FuzzyScore,
        Self::CosMaxChunk,
        Self::CosMeanChunk,
        Self::RrfScore,
        Self::NumSourcesHit,
        Self::BestSource,
        Self::SenderAffinity,
        Self::UserRepliedThread,
        Self::PriorOpensFromSender,
        Self::ThreadActivity,
        Self::AgeDays,
        Self::RecencyDecay,
        Self::MatchesDateIntent,
        Self::IsUnread,
        Self::IsFlagged,
        Self::IsPinned,
        Self::AiPriority,
        Self::HasTagMatch,
        Self::FolderPrior,
        Self::HasAttachmentMatch,
        Self::IsThreadRoot,
        Self::ThreadSize,
        Self::MsgLength,
        Self::SenderReputation,
        Self::IsNewsletter,
        Self::IsAutomated,
    ];

    /// The exact string prd.md's Stage 3 table uses — the key an external
    /// consumer (a TOML weights file, a logged training row, `RankExplanation`)
    /// actually stores, so it never has to know this enum's Rust spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bm25Subject => "bm25_subject",
            Self::Bm25Body => "bm25_body",
            Self::Bm25From => "bm25_from",
            Self::Bm25Attach => "bm25_attach",
            Self::ExactPhraseHit => "exact_phrase_hit",
            Self::TermCoverage => "term_coverage",
            Self::ProximityMinSpan => "proximity_min_span",
            Self::BestMatchField => "best_match_field",
            Self::FuzzyScore => "fuzzy_score",
            Self::CosMaxChunk => "cos_max_chunk",
            Self::CosMeanChunk => "cos_mean_chunk",
            Self::RrfScore => "rrf_score",
            Self::NumSourcesHit => "num_sources_hit",
            Self::BestSource => "best_source",
            Self::SenderAffinity => "sender_affinity",
            Self::UserRepliedThread => "user_replied_thread",
            Self::PriorOpensFromSender => "prior_opens_from_sender",
            Self::ThreadActivity => "thread_activity",
            Self::AgeDays => "age_days",
            Self::RecencyDecay => "recency_decay",
            Self::MatchesDateIntent => "matches_date_intent",
            Self::IsUnread => "is_unread",
            Self::IsFlagged => "is_flagged",
            Self::IsPinned => "is_pinned",
            Self::AiPriority => "ai_priority",
            Self::HasTagMatch => "has_tag_match",
            Self::FolderPrior => "folder_prior",
            Self::HasAttachmentMatch => "has_attachment_match",
            Self::IsThreadRoot => "is_thread_root",
            Self::ThreadSize => "thread_size",
            Self::MsgLength => "msg_length",
            Self::SenderReputation => "sender_reputation",
            Self::IsNewsletter => "is_newsletter",
            Self::IsAutomated => "is_automated",
        }
    }

    /// Which of prd.md's Stage 3 groups this feature belongs to — see the
    /// module docs for why `fusion` is a real eighth group rather than folded
    /// into `textual`.
    #[must_use]
    pub const fn group(self) -> FeatureGroup {
        match self {
            Self::Bm25Subject
            | Self::Bm25Body
            | Self::Bm25From
            | Self::Bm25Attach
            | Self::ExactPhraseHit
            | Self::TermCoverage
            | Self::ProximityMinSpan
            | Self::BestMatchField
            | Self::FuzzyScore => FeatureGroup::Textual,
            Self::CosMaxChunk | Self::CosMeanChunk => FeatureGroup::Semantic,
            Self::RrfScore | Self::NumSourcesHit | Self::BestSource => FeatureGroup::Fusion,
            Self::SenderAffinity
            | Self::UserRepliedThread
            | Self::PriorOpensFromSender
            | Self::ThreadActivity => FeatureGroup::Personal,
            Self::AgeDays | Self::RecencyDecay | Self::MatchesDateIntent => FeatureGroup::Temporal,
            Self::IsUnread
            | Self::IsFlagged
            | Self::IsPinned
            | Self::AiPriority
            | Self::HasTagMatch
            | Self::FolderPrior => FeatureGroup::Status,
            Self::HasAttachmentMatch | Self::IsThreadRoot | Self::ThreadSize | Self::MsgLength => {
                FeatureGroup::Structural
            }
            Self::SenderReputation | Self::IsNewsletter | Self::IsAutomated => FeatureGroup::Global,
        }
    }
}

impl fmt::Display for FeatureName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests;
