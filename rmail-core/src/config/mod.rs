//! Typed configuration for rmail.
//!
//! [`Config`] models the master TOML described in the PRD. Loading is a pure
//! function of `(file, environment)` — there are no globals, so a fresh parse
//! can be produced at any time for hot reload. Unknown keys are rejected
//! (`deny_unknown_fields`) rather than silently ignored, and secret material is
//! never inlined: accounts reference credentials via `password_command`,
//! `password_env`, or `keychain` only. [`RankWeights`] is this rule's one
//! deliberate exception — see its own doc comment for why an *open*
//! feature-name-keyed table, validated one layer up in
//! [`crate::rank::l1::Weights::from_config`] rather than here, is correct
//! for that specific field.
//!
//! # Environment overlay
//!
//! Any field can be overridden by an environment variable prefixed `RMAIL_`
//! with `__` separating nesting levels, e.g. `RMAIL_AI__ENABLED=false` or
//! `RMAIL_GRPC__AUTH=none`. Only variables whose first segment names a known
//! top-level table are considered, so unrelated `RMAIL_*` vars (such as the
//! daemon's `RMAIL_SOCKET`) are ignored rather than causing an unknown-key
//! error.

mod duration;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use figment::providers::{Env, Format, Toml};
use figment::{Figment, Provider};
use serde::Deserialize;

pub use duration::{parse_human_duration, HumanDuration};

/// Top-level table names accepted from the environment overlay.
const KNOWN_TABLES: &[&str] = &[
    "accounts", "sync", "search", "index", "ai", "tags", "notes", "send", "finder", "grpc",
];

/// Errors produced while loading or parsing configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The requested config file does not exist.
    #[error("config file not found: {0}")]
    NotFound(PathBuf),

    /// The configuration was structurally invalid (unknown key, bad value,
    /// type mismatch, malformed TOML, ...).
    ///
    /// The `figment::Error` is boxed to keep `ConfigError` small.
    #[error("invalid configuration: {0}")]
    Invalid(#[source] Box<figment::Error>),
}

impl From<figment::Error> for ConfigError {
    fn from(source: figment::Error) -> Self {
        Self::Invalid(Box::new(source))
    }
}

// ---------------------------------------------------------------------------
// Constrained-value enums (each rejects unrecognized values with a clear error)
// ---------------------------------------------------------------------------

/// Search execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Lexical (FTS5/BM25) only.
    Lexical,
    /// Dense-vector only.
    Semantic,
    /// Fused lexical + semantic (+ the rest of the cascade).
    Hybrid,
}

/// Candidate-list fusion strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fusion {
    /// Reciprocal Rank Fusion.
    Rrf,
    /// Normalized weighted linear blend.
    Linear,
}

/// L2 reranking backend selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rerank {
    /// No rerank; L1 order stands.
    Off,
    /// Local ONNX cross-encoder.
    CrossEncoder,
    /// Claude listwise rerank.
    Claude,
    /// Cross-encoder interactive, Claude for deep search.
    Auto,
}

/// AI provider backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    /// Anthropic Claude via the Messages API.
    Claude,
    /// Fully-local on-device inference.
    Local,
}

/// Embedding backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingBackend {
    /// Local ONNX embeddings (offline, default for privacy).
    Local,
    /// Claude-hosted embeddings.
    Claude,
}

/// Behavior when an AI spend/token cap is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnCap {
    /// Pause AI processing entirely.
    Pause,
    /// Continue only cheap triage.
    TriageOnly,
    /// Drop further AI work silently.
    Drop,
}

/// A data-residency/AI-eligibility classification for an account, folder, or
/// pattern, resolved by [`crate::ai::policy::PolicyEngine`].
///
/// Declared in ascending order of restrictiveness — [`Ord`]/[`PartialOrd`] are
/// derived from that order deliberately, so `a.max(b)` picks the more
/// restrictive of two conflicting classifications. The policy engine relies on
/// exactly this ordering to break ties between same-specificity rules (its
/// "deny wins" rule; see the `ai::policy` module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiPolicyMode {
    /// May be sent to the configured cloud provider (subject to the
    /// redaction firewall, task 44) and may be used for on-device inference.
    Allowed,
    /// May only be processed by the on-device inference path; must never
    /// reach a cloud provider.
    LocalOnly,
    /// Invisible to every AI feature: not analyzed, not listed in an
    /// AI-facing query, not embedded, not retrieved. See the `ai::policy`
    /// module docs for what "invisible" means structurally, not just as a
    /// denial.
    Forbidden,
}

impl AiPolicyMode {
    /// The wire/TOML form of this mode (`"allowed"`, `"local_only"`,
    /// `"forbidden"`) — what `ai.policy.rules[].mode` accepts in the config
    /// file and what a log line should show, rather than Rust's `Debug`
    /// capitalization.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            AiPolicyMode::Allowed => "allowed",
            AiPolicyMode::LocalOnly => "local_only",
            AiPolicyMode::Forbidden => "forbidden",
        }
    }
}

/// Semantic-index embedding provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticProvider {
    /// Voyage AI hosted embeddings.
    Voyage,
    /// Local ONNX embeddings (offline).
    Local,
    /// Semantic indexing disabled.
    None,
}

/// Named-entity-recognition backend for the entity index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NerBackend {
    /// Claude-based NER.
    Claude,
    /// Local NER model.
    Local,
    /// Regex extractors only.
    None,
}

/// gRPC authentication mode for TCP listeners.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrpcAuth {
    /// No authentication (only valid with `--insecure`).
    None,
    /// Bearer capability tokens.
    Token,
    /// Mutual TLS.
    Mtls,
}

/// Default tag ⇄ IMAP synchronization mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TagSyncMode {
    /// Round-trip to IMAP, downgrade to local on failure.
    Auto,
    /// Always round-trip to IMAP keywords/labels.
    Imap,
    /// Local-only tags.
    Local,
}

/// Default fuzzy-finder scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinderScope {
    /// All sources.
    All,
    /// Messages only.
    Messages,
    /// Contacts only.
    Contacts,
    /// Folders only.
    Folders,
    /// Tags only.
    Tags,
    /// Saved searches only.
    SavedSearches,
    /// Commands only.
    Commands,
}

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

/// The complete rmail configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Configured IMAP/SMTP accounts.
    pub accounts: Vec<AccountConfig>,
    /// Background synchronization settings.
    pub sync: SyncConfig,
    /// Retrieval & ranking settings.
    pub search: SearchConfig,
    /// Indexing pipeline settings.
    pub index: IndexConfig,
    /// AI/Claude bridge settings.
    pub ai: AiConfig,
    /// Tagging settings.
    pub tags: TagsConfig,
    /// Notes settings.
    pub notes: NotesConfig,
    /// Compose/send/outbox settings.
    pub send: SendConfig,
    /// Fuzzy-finder settings.
    pub finder: FinderConfig,
    /// gRPC server settings.
    pub grpc: GrpcConfig,
}

impl Config {
    /// Load and validate configuration from `path`, overlaid with the
    /// environment (see the [module docs](self)).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NotFound`] if the file is absent, or
    /// [`ConfigError::Invalid`] if the merged configuration is malformed.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(ConfigError::NotFound(path.to_path_buf()));
        }
        Self::figment(Toml::file(path))
            .extract()
            .map_err(Into::into)
    }

    /// Load configuration from `path` when present, else start from defaults;
    /// the environment overlay is applied in both cases.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] if the merged configuration is
    /// malformed. A missing file is not an error.
    pub fn load_or_default(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        Self::figment(Toml::file(path.as_ref()))
            .extract()
            .map_err(Into::into)
    }

    /// Parse and validate configuration from a TOML string, overlaid with the
    /// environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] if the TOML is malformed or contains an
    /// unknown key or bad value.
    pub fn from_toml_str(toml: &str) -> Result<Self, ConfigError> {
        Self::figment(Toml::string(toml))
            .extract()
            .map_err(Into::into)
    }

    fn figment<P: Provider>(source: P) -> Figment {
        Figment::new().merge(source).merge(env_overlay())
    }
}

/// The environment overlay provider: `RMAIL_`-prefixed, `__`-nested, restricted
/// to variables whose first segment names a known top-level table.
fn env_overlay() -> Env {
    Env::prefixed("RMAIL_")
        .filter_map(|key| {
            // A valid override is `RMAIL_<table>__<field>...`. Split on the `__`
            // nesting separator (not single `_`, which appears inside field
            // names like `bm25_weights`) and keep only vars whose first segment
            // names a known table. This drops unrelated `RMAIL_*` vars (e.g. the
            // daemon's `RMAIL_SOCKET`) *and* single-underscore vars that would
            // otherwise land as unknown top-level keys and hard-fail the load.
            // Env var names keep their case, so compare case-insensitively.
            let head = key
                .as_str()
                .split("__")
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if KNOWN_TABLES.contains(&head.as_str()) {
                Some(key.into())
            } else {
                None
            }
        })
        .split("__")
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

/// A single IMAP/SMTP account. Credentials are referenced indirectly — inline
/// passwords are rejected as unknown keys.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    /// Human-readable account name (unique).
    pub name: String,
    /// IMAP server hostname.
    #[serde(default)]
    pub imap_server: Option<String>,
    /// IMAP port.
    #[serde(default = "default_imap_port")]
    pub port: u16,
    /// Login username.
    #[serde(default)]
    pub username: Option<String>,
    /// Shell command whose stdout yields the password.
    #[serde(default)]
    pub password_command: Option<String>,
    /// Environment variable holding the password.
    #[serde(default)]
    pub password_env: Option<String>,
    /// macOS Keychain reference (service name) holding the password.
    #[serde(default)]
    pub keychain: Option<String>,
    /// SMTP server hostname.
    #[serde(default)]
    pub smtp_server: Option<String>,
    /// SMTP port.
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    /// Per-account AI overrides.
    #[serde(default)]
    pub ai: AccountAiConfig,
}

const fn default_imap_port() -> u16 {
    993
}
const fn default_smtp_port() -> u16 {
    587
}

/// Per-account AI settings (allows hard opt-out and residency tagging).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AccountAiConfig {
    /// Whether any AI processing is permitted for this account.
    pub enabled: bool,
    /// Optional data-residency tag consulted by the AI policy engine.
    pub residency: Option<String>,
}

impl Default for AccountAiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            residency: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

/// Background synchronization settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SyncConfig {
    /// Poll interval when IDLE is active (also the IDLE keepalive cadence).
    pub interval: HumanDuration,
    /// Whether to use IMAP IDLE push.
    pub idle: bool,
    /// Whether to use CONDSTORE/QRESYNC delta sync.
    pub qresync: bool,
    /// Poll interval used when IDLE is unavailable.
    pub poll_interval: HumanDuration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            interval: HumanDuration::new(mins(5)),
            idle: true,
            qresync: true,
            poll_interval: HumanDuration::new(mins(5)),
        }
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Retrieval & ranking settings (Part I of the PRD).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SearchConfig {
    /// Default execution mode.
    pub default_mode: SearchMode,
    /// Fusion strategy.
    pub fusion: Fusion,
    /// RRF `k` constant.
    pub rrf_k: u32,
    /// Rerank backend.
    pub rerank: Rerank,
    /// Candidates each retriever returns.
    pub candidates_per_source: u32,
    /// Candidates kept for L2 rerank.
    pub top_k_rerank: u32,
    /// Whether implicit-feedback personalization is on.
    pub learning: bool,
    /// MMR lambda for exploratory diversification.
    pub mmr_lambda: f64,
    /// Default result limit.
    pub default_limit: u32,
    /// Field-weighted BM25 weights.
    pub bm25_weights: Bm25Weights,
    /// Intent-dependent fusion source weights.
    pub fusion_weights: FusionWeights,
    /// Cold-start deterministic ranker weight *overrides* — see
    /// [`RankWeights`]'s own doc comment.
    pub rank_weights: RankWeights,
    /// L2 reranker settings.
    pub reranker: RerankerConfig,
    /// Query-expansion settings.
    pub expansion: ExpansionConfig,
    /// Candidate-generation retriever toggles (task 28).
    pub retrievers: RetrieversConfig,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_mode: SearchMode::Hybrid,
            fusion: Fusion::Rrf,
            rrf_k: 60,
            rerank: Rerank::Auto,
            candidates_per_source: 200,
            top_k_rerank: 50,
            learning: true,
            mmr_lambda: 0.7,
            default_limit: 25,
            bm25_weights: Bm25Weights::default(),
            fusion_weights: FusionWeights::default(),
            rank_weights: RankWeights::default(),
            reranker: RerankerConfig::default(),
            expansion: ExpansionConfig::default(),
            retrievers: RetrieversConfig::default(),
        }
    }
}

/// Which Stage 1 candidate-generation retrievers run, and their tunables.
///
/// prd.md, Stage 1: "Each is individually skippable (config/degradation)."
/// `retrieve::lexical` is not toggleable — it is the baseline recall every
/// intent depends on, and disabling it would leave a plain keyword query with
/// nothing to rank on at all. The six retrievers named here are exactly
/// prd.md's own list for this task ("Dense kNN..., fuzzy..., entity match,
/// structured filter..., prefix/autocomplete, and recency-prior
/// retrievers... individually skippable").
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RetrieversConfig {
    /// Dense-vector kNN retriever.
    pub dense: bool,
    /// nucleo subsequence fuzzy retriever.
    pub fuzzy: bool,
    /// Entity/entity_mentions exact-match retriever.
    pub entity: bool,
    /// Structured hard-filter (pass/fail) retriever.
    pub structured: bool,
    /// FTS5 prefix/autocomplete retriever.
    pub prefix: bool,
    /// Recency-decay prior retriever.
    pub recency: bool,
    /// Half-life, in days, for the recency retriever's
    /// `exp(-age_days/half_life)` decay score.
    pub recency_half_life_days: f64,
}

impl Default for RetrieversConfig {
    fn default() -> Self {
        Self {
            dense: true,
            fuzzy: true,
            entity: true,
            structured: true,
            prefix: true,
            recency: true,
            // Matches `[finder.ranking].half_life_days`'s own default
            // (prd.md, Part III) — prd.md gives Part I's recency prior no
            // distinct default of its own, and finder's is the same decay
            // shape over the same underlying signal (message recency).
            recency_half_life_days: 30.0,
        }
    }
}

/// Field-weighted BM25 column weights.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Bm25Weights {
    /// Subject weight.
    pub subject: f64,
    /// From weight.
    pub from: f64,
    /// To/Cc weight.
    pub to: f64,
    /// Body weight.
    pub body: f64,
    /// Attachment-text weight.
    pub attachments: f64,
    /// Notes weight.
    pub notes: f64,
    /// AI-summary weight.
    pub ai_summary: f64,
}

impl Default for Bm25Weights {
    fn default() -> Self {
        Self {
            subject: 8.0,
            from: 4.0,
            to: 2.0,
            body: 1.0,
            attachments: 1.0,
            notes: 3.0,
            ai_summary: 2.0,
        }
    }
}

/// Intent-dependent fusion weights.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FusionWeights {
    /// Weights for navigational/known-item intent.
    pub navigational: FusionSourceWeights,
    /// Weights for exploratory/topical intent.
    pub exploratory: FusionSourceWeights,
    /// Weights for lookup/entity intent.
    pub lookup: FusionSourceWeights,
}

impl Default for FusionWeights {
    fn default() -> Self {
        Self {
            navigational: FusionSourceWeights {
                lexical: 1.0,
                dense: 0.6,
                fuzzy: 0.9,
                entity: 0.7,
                recency: 0.8,
            },
            exploratory: FusionSourceWeights {
                lexical: 0.7,
                dense: 1.0,
                fuzzy: 0.4,
                entity: 0.5,
                recency: 0.3,
            },
            lookup: FusionSourceWeights {
                lexical: 0.8,
                dense: 0.5,
                fuzzy: 0.6,
                entity: 1.0,
                recency: 0.4,
            },
        }
    }
}

/// Per-source fusion weights for one intent.
///
/// A partially-specified intent table fills unspecified sources with a neutral
/// `1.0` (equal weighting) rather than the intent's tuned defaults, since those
/// defaults are per-intent and cannot be expressed by a single `Default`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FusionSourceWeights {
    /// Lexical BM25 source weight.
    pub lexical: f64,
    /// Dense-vector source weight.
    pub dense: f64,
    /// Fuzzy source weight.
    pub fuzzy: f64,
    /// Entity source weight.
    pub entity: f64,
    /// Recency-prior source weight.
    pub recency: f64,
}

impl Default for FusionSourceWeights {
    fn default() -> Self {
        Self {
            lexical: 1.0,
            dense: 1.0,
            fuzzy: 1.0,
            entity: 1.0,
            recency: 1.0,
        }
    }
}

/// Cold-start deterministic ranker (prd.md Stage 4) weight *overrides* —
/// "All weights are TOML-overridable" (prd.md). Keyed by the same stable
/// strings [`crate::features::FeatureName::as_str`] produces (`"bm25_subject"`,
/// `"is_newsletter"`, ...), not by a fixed set of Rust field names: `config`
/// has no dependency on `features`/`rank` (the opposite dependency direction
/// holds already — `features::extract` reads [`Bm25Weights`] from this very
/// module), so a key here cannot be validated against the real
/// `FeatureName` enum at this layer. [`crate::rank::l1::Weights::from_config`]
/// does that validation one layer up, rejecting a key that names no real
/// feature (or a `NaN`/`±inf` value) with a clear error. **This crate does
/// not call it automatically anywhere yet** — no gRPC service builds a
/// `Ranker` from a loaded [`Config`] today, since that wiring belongs to a
/// `SearchService` this workspace does not have until a later task. A
/// successful [`Config::load`] is therefore *not* proof that
/// `[search.rank_weights]` is well-formed; whatever eventually builds the
/// live `Ranker` must call [`crate::rank::l1::Weights::from_config`] itself
/// and handle its `Result`, the same way [`Config::load`]'s own callers
/// already have to handle a [`ConfigError`].
///
/// Empty (the default, `[search.rank_weights]` omitted entirely) means "use
/// [`crate::rank::l1::Weights::default`]'s built-in PRD cold-start table
/// unmodified." A key present here overrides just that one feature's weight
/// — this table is a *sparse* patch on top of the built-in seventeen, not a
/// replacement for them, so tuning one weight in `rmail.toml` never requires
/// restating the other sixteen.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct RankWeights(pub BTreeMap<String, f64>);

/// L2 reranker settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RerankerConfig {
    /// Local ONNX cross-encoder model name.
    pub cross_encoder_model: String,
    /// Claude rerank model id.
    pub claude_model: String,
    /// Max candidates sent to Claude for listwise rerank.
    pub claude_max_candidates: u32,
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            cross_encoder_model: "bge-reranker-base".to_owned(),
            claude_model: "claude-haiku-4-5".to_owned(),
            claude_max_candidates: 30,
        }
    }
}

/// Query-expansion settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExpansionConfig {
    /// Local co-occurrence synonym expansion.
    pub synonyms: bool,
    /// Claude query expansion (opt-in, cached).
    pub claude: bool,
    /// SymSpell/trigram spelling correction.
    pub spellfix: bool,
}

impl Default for ExpansionConfig {
    fn default() -> Self {
        Self {
            synonyms: true,
            claude: false,
            spellfix: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

/// Indexing pipeline settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexConfig {
    /// Whether indexing runs.
    pub enabled: bool,
    /// Worker count for the index pipeline.
    pub workers: u32,
    /// Chunks per embed request.
    pub batch_size: u32,
    /// Recent mail (days) boosted to the front of the queue.
    pub priority_recent_days: u32,
    /// Mailboxes prioritized first.
    pub priority_mailboxes: Vec<String>,
    /// Lexical (FTS5) settings.
    pub lexical: IndexLexicalConfig,
    /// Text/attachment extraction settings.
    pub extract: IndexExtractConfig,
    /// Semantic (vector) settings.
    pub semantic: IndexSemanticConfig,
    /// Entity-extraction settings.
    pub entities: IndexEntitiesConfig,
    /// Index-local search defaults.
    pub search: IndexSearchConfig,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            workers: 4,
            batch_size: 64,
            priority_recent_days: 30,
            priority_mailboxes: vec!["INBOX".to_owned()],
            lexical: IndexLexicalConfig::default(),
            extract: IndexExtractConfig::default(),
            semantic: IndexSemanticConfig::default(),
            entities: IndexEntitiesConfig::default(),
            search: IndexSearchConfig::default(),
        }
    }
}

/// Lexical FTS5 index settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexLexicalConfig {
    /// Whether the lexical index is maintained.
    pub enabled: bool,
    /// FTS5 tokenizer directive.
    pub tokenizer: String,
    /// Field weights for the lexical index.
    pub weights: LexicalWeights,
}

impl Default for IndexLexicalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tokenizer: "unicode61 remove_diacritics 2".to_owned(),
            weights: LexicalWeights::default(),
        }
    }
}

/// Lexical index field weights.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LexicalWeights {
    /// Subject weight.
    pub subject: f64,
    /// Sender weight.
    pub sender: f64,
    /// Recipients weight.
    pub recipients: f64,
    /// Body weight.
    pub body: f64,
    /// Attachment-text weight.
    pub attachments: f64,
    /// Notes weight.
    pub notes: f64,
    /// AI-summary weight.
    pub summary: f64,
}

impl Default for LexicalWeights {
    fn default() -> Self {
        Self {
            subject: 8.0,
            sender: 4.0,
            recipients: 2.0,
            body: 1.0,
            attachments: 1.0,
            notes: 3.0,
            summary: 2.0,
        }
    }
}

/// Text/attachment extraction settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexExtractConfig {
    /// Strip HTML to text before indexing.
    pub strip_html: bool,
    /// Extract attachment text.
    pub attachments: bool,
    /// OCR images/scanned PDFs (opt-in).
    pub ocr: bool,
    /// OCR languages.
    pub ocr_langs: Vec<String>,
    /// Maximum attachment size to extract (MiB).
    pub max_attachment_mb: u32,
    /// Attachment formats to extract.
    pub formats: Vec<String>,
}

impl Default for IndexExtractConfig {
    fn default() -> Self {
        Self {
            strip_html: true,
            attachments: true,
            ocr: false,
            ocr_langs: vec!["eng".to_owned()],
            max_attachment_mb: 25,
            // Exactly the names `attach::extract::Format::as_str` produces.
            // `"eml"` used to be here and matches no format at all, while
            // `"html"` was absent — so an HTML attachment was recorded
            // `Unsupported` under the shipped configuration and, because that
            // status is not retryable, stayed unsearchable for ever. The whole
            // suite ran against a config that overrode this list, which is why
            // nothing noticed.
            formats: ["pdf", "docx", "xlsx", "pptx", "html", "csv", "txt"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

/// Semantic (vector) index settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexSemanticConfig {
    /// Whether semantic indexing runs.
    pub enabled: bool,
    /// Embedding provider (defaults to local for privacy).
    pub provider: SemanticProvider,
    /// Tokens per chunk.
    pub chunk_tokens: u32,
    /// Overlap tokens between chunks.
    pub chunk_overlap: u32,
    /// Embed thread summaries.
    pub embed_threads: bool,
    /// Embed attachment text.
    pub index_attachments: bool,
    /// Voyage backend settings.
    pub voyage: VoyageConfig,
    /// Local backend settings.
    pub local: LocalEmbedConfig,
}

impl Default for IndexSemanticConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: SemanticProvider::Local,
            chunk_tokens: 512,
            chunk_overlap: 64,
            embed_threads: true,
            index_attachments: true,
            voyage: VoyageConfig::default(),
            local: LocalEmbedConfig::default(),
        }
    }
}

/// Voyage embedding backend settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct VoyageConfig {
    /// Voyage model id.
    pub model: String,
    /// Embedding dimensionality.
    pub dim: u32,
    /// Command yielding the Voyage API key.
    pub api_key_command: String,
    /// Requests per minute.
    pub rpm: u32,
}

impl Default for VoyageConfig {
    fn default() -> Self {
        Self {
            model: "voyage-3".to_owned(),
            dim: 1024,
            api_key_command: "security find-generic-password -s voyage -w".to_owned(),
            rpm: 300,
        }
    }
}

/// Local embedding backend settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LocalEmbedConfig {
    /// Local model id.
    pub model: String,
    /// Embedding dimensionality.
    pub dim: u32,
    /// Where model weights live.
    ///
    /// Empty means the default: `$RMAIL_MODEL_CACHE`, else
    /// `$XDG_CACHE_HOME/rmail/models`, else `~/.cache/rmail/models`. Settable
    /// here so the path is part of the validated configuration rather than only
    /// an environment variable the daemon happens to read.
    pub cache_dir: String,
    /// Whether the daemon may fetch missing model weights itself.
    ///
    /// Off by default. The point of the local backend is that nothing leaves
    /// the host, and a daemon that silently contacts Hugging Face the first
    /// time somebody searches does not honor that — nor is the fetch
    /// suppressible from outside, because the downloader ignores
    /// `HF_HUB_OFFLINE`. Provisioning is therefore an explicit act: turn this
    /// on once, or populate the cache directory out of band.
    pub allow_download: bool,
}

impl Default for LocalEmbedConfig {
    fn default() -> Self {
        Self {
            model: "bge-small-en-v1.5".to_owned(),
            dim: 384,
            cache_dir: String::new(),
            allow_download: false,
        }
    }
}

/// Entity-extraction settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexEntitiesConfig {
    /// Whether entity extraction runs.
    pub enabled: bool,
    /// Deterministic regex extractors.
    pub regex: bool,
    /// NER backend.
    pub ner: NerBackend,
    /// NER model id (when `ner = claude`).
    pub ner_model: String,
    /// Minimum confidence to keep an entity mention.
    pub min_confidence: f64,
}

impl Default for IndexEntitiesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            regex: true,
            ner: NerBackend::Claude,
            ner_model: "claude-haiku-4-5".to_owned(),
            min_confidence: 0.5,
        }
    }
}

/// Index-local search defaults.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexSearchConfig {
    /// Default mode.
    pub mode: SearchMode,
    /// RRF `k`.
    pub rrf_k: u32,
    /// Default limit.
    pub default_limit: u32,
}

impl Default for IndexSearchConfig {
    fn default() -> Self {
        Self {
            mode: SearchMode::Hybrid,
            rrf_k: 60,
            default_limit: 25,
        }
    }
}

// ---------------------------------------------------------------------------
// AI
// ---------------------------------------------------------------------------

/// AI/Claude bridge settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiConfig {
    /// Whether AI features run.
    pub enabled: bool,
    /// AI provider.
    pub provider: AiProvider,
    /// Command yielding the provider API key.
    pub api_key_command: String,
    /// Model routing.
    pub models: AiModels,
    /// Deep-pass gating.
    pub deep_pass: AiDeepPass,
    /// Concurrency/cost limits.
    pub limits: AiLimits,
    /// Batch-API settings.
    pub batching: AiBatching,
    /// Prompt-cache settings.
    pub prompt_cache: AiPromptCache,
    /// Retry/backoff settings.
    pub retry: AiRetry,
    /// Privacy/redaction settings.
    pub privacy: AiPrivacy,
    /// Data-residency / per-account/folder/pattern AI eligibility rules.
    pub policy: AiPolicyConfig,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: AiProvider::Claude,
            api_key_command: "security find-generic-password -s anthropic -w".to_owned(),
            models: AiModels::default(),
            deep_pass: AiDeepPass::default(),
            limits: AiLimits::default(),
            batching: AiBatching::default(),
            prompt_cache: AiPromptCache::default(),
            retry: AiRetry::default(),
            privacy: AiPrivacy::default(),
            policy: AiPolicyConfig::default(),
        }
    }
}

/// AI model routing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiModels {
    /// Triage model (cheap, high-volume).
    pub triage: String,
    /// Deep-pass model.
    pub deep: String,
    /// Embedding backend.
    pub embedding: EmbeddingBackend,
}

impl Default for AiModels {
    fn default() -> Self {
        Self {
            triage: "claude-haiku-4-5".to_owned(),
            deep: "claude-opus-4-8".to_owned(),
            embedding: EmbeddingBackend::Local,
        }
    }
}

/// Deep-pass gating conditions.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiDeepPass {
    /// Minimum triage priority that triggers a deep pass.
    pub on_priority: String,
    /// Trigger a deep pass when triage flags needs-reply.
    pub on_needs_reply: bool,
    /// Categories that always get a deep pass.
    pub categories: Vec<String>,
    /// Generate a suggested reply in the deep pass.
    pub suggest_reply: bool,
}

impl Default for AiDeepPass {
    fn default() -> Self {
        Self {
            on_priority: "high".to_owned(),
            on_needs_reply: true,
            categories: ["work", "personal", "invoice", "receipt"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            suggest_reply: true,
        }
    }
}

/// AI concurrency and spend limits.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiLimits {
    /// Maximum concurrent AI requests.
    pub max_concurrency: u32,
    /// Requests-per-minute cap.
    pub requests_per_minute: u32,
    /// Daily token cap.
    pub daily_token_cap: u64,
    /// Daily USD cost cap.
    pub daily_cost_cap_usd: f64,
    /// Monthly USD cost cap.
    pub monthly_cost_cap_usd: f64,
    /// Behavior when a cap is hit.
    pub on_cap: OnCap,
}

impl Default for AiLimits {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            requests_per_minute: 60,
            daily_token_cap: 2_000_000,
            daily_cost_cap_usd: 5.00,
            monthly_cost_cap_usd: 100.00,
            on_cap: OnCap::Pause,
        }
    }
}

/// Message Batches API settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiBatching {
    /// Whether batch mode is enabled.
    pub enabled: bool,
    /// Queue depth at which batch mode engages.
    pub threshold: u32,
    /// Maximum items per batch.
    pub max_batch: u32,
}

impl Default for AiBatching {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 200,
            max_batch: 5000,
        }
    }
}

/// Prompt-cache settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiPromptCache {
    /// Whether prompt caching is enabled.
    pub enabled: bool,
    /// Cache time-to-live.
    pub ttl: HumanDuration,
}

impl Default for AiPromptCache {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl: HumanDuration::new(mins(60)),
        }
    }
}

/// Retry/backoff settings for provider calls.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiRetry {
    /// Maximum attempts.
    pub max_attempts: u32,
    /// Base backoff (ms).
    pub base_delay_ms: u64,
    /// Maximum backoff (ms).
    pub max_delay_ms: u64,
}

impl Default for AiRetry {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay_ms: 1000,
            max_delay_ms: 60000,
        }
    }
}

/// Privacy/redaction settings for outbound AI payloads.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiPrivacy {
    /// Whether the PII redaction firewall runs.
    pub redact: bool,
    /// Redaction pattern names to enforce.
    pub redact_patterns: Vec<String>,
    /// Strip attachments from AI payloads.
    pub strip_attachments: bool,
    /// Maximum body characters sent to the model.
    pub max_body_chars: u32,
}

impl Default for AiPrivacy {
    fn default() -> Self {
        Self {
            redact: true,
            redact_patterns: ["ssn", "credit_card", "iban", "api_key", "otp"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            strip_attachments: true,
            max_body_chars: 40000,
        }
    }
}

/// Data-residency / AI-eligibility policy settings — the declarative rule
/// table [`crate::ai::policy::PolicyEngine`] resolves against.
///
/// `default_mode` and `default_residency` govern any account/folder no rule
/// below names explicitly; see the `ai::policy` module docs for why
/// [`AiPolicyMode::Allowed`] is the shipped default and how that stays safe.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiPolicyConfig {
    /// Classification applied when no rule below names the account/folder.
    pub default_mode: AiPolicyMode,
    /// Residency tag applied when no rule below (or the matching rule
    /// itself) names one.
    pub default_residency: String,
    /// Declarative account/folder/pattern rules, most specific match wins;
    /// see the `ai::policy` module docs for the exact precedence order.
    pub rules: Vec<AiPolicyRule>,
}

impl Default for AiPolicyConfig {
    fn default() -> Self {
        Self {
            default_mode: AiPolicyMode::Allowed,
            default_residency: "unspecified".to_owned(),
            rules: Vec::new(),
        }
    }
}

/// One declarative AI-policy rule.
///
/// Exactly one of `account`/`folder` may be omitted, never both — a rule
/// naming neither would be indistinguishable from `ai.policy.default_mode`
/// and is rejected by [`crate::ai::policy::PolicyEngine`] rather than
/// silently accepted as a no-op. `mode` carries no default: a rule that does
/// not say what it classifies its target as is a configuration mistake, not
/// something to paper over with an implicit fallback.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AiPolicyRule {
    /// Account this rule applies to. `None` matches every account — only
    /// meaningful paired with `folder`, since an account-less,
    /// folder-less rule is rejected (see above).
    #[serde(default)]
    pub account: Option<String>,
    /// Folder/mailbox this rule applies to. A glob (`*` = any run of
    /// characters, `?` = exactly one — no `[...]` character classes) makes
    /// this a pattern rule; a plain string makes it an exact-folder rule
    /// (more specific than a pattern, see the `ai::policy` module docs).
    /// `None` makes this an account-wide rule.
    #[serde(default)]
    pub folder: Option<String>,
    /// The classification this rule assigns.
    pub mode: AiPolicyMode,
    /// Residency tag this rule assigns; falls back to
    /// `ai.policy.default_residency` when unset.
    #[serde(default)]
    pub residency: Option<String>,
    /// Free-text justification surfaced by `PolicyEngine::explain` (e.g. "
    /// attorney-client privileged correspondence").
    #[serde(default)]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Tags & notes
// ---------------------------------------------------------------------------

/// Tagging settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TagsConfig {
    /// Tag color palette.
    pub palette: Vec<String>,
    /// Hierarchy separator character.
    pub hierarchy_separator: String,
    /// Default tag ⇄ IMAP sync mode.
    pub default_sync_mode: TagSyncMode,
    /// IMAP keyword/label mapping settings.
    pub imap: TagsImap,
    /// AI auto-tagging settings.
    pub ai: TagsAi,
}

impl Default for TagsConfig {
    fn default() -> Self {
        Self {
            palette: [
                "#7aa2f7", "#e0af68", "#9ece6a", "#f7768e", "#bb9af7", "#7dcfff",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
            hierarchy_separator: "/".to_owned(),
            default_sync_mode: TagSyncMode::Auto,
            imap: TagsImap::default(),
            ai: TagsAi::default(),
        }
    }
}

/// Tag ⇄ IMAP keyword/label mapping settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TagsImap {
    /// Prefix applied to synced IMAP keywords.
    pub keyword_prefix: String,
    /// Map system flags to tags.
    pub map_system: bool,
    /// Use Gmail `X-GM-LABELS`.
    pub gmail_labels: bool,
}

impl Default for TagsImap {
    fn default() -> Self {
        Self {
            keyword_prefix: "rmail/".to_owned(),
            map_system: true,
            gmail_labels: true,
        }
    }
}

/// AI auto-tagging settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TagsAi {
    /// Whether AI tag suggestions run.
    pub enabled: bool,
    /// Suggestion model id.
    pub model: String,
    /// Suggest tags on newly synced mail.
    pub suggest_on_new_mail: bool,
    /// Maximum suggestions per message.
    pub max_suggestions: u32,
    /// Confidence at/above which a suggested tag is auto-applied.
    pub auto_apply_min_confidence: f64,
    /// NL-defined tag taxonomy the classifier draws from.
    pub taxonomy: Vec<String>,
}

impl Default for TagsAi {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "claude-haiku-4-5".to_owned(),
            suggest_on_new_mail: true,
            max_suggestions: 3,
            auto_apply_min_confidence: 0.85,
            taxonomy: [
                "work",
                "personal",
                "finance/invoice",
                "finance/receipt",
                "travel",
                "newsletter",
                "urgent",
                "follow-up",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        }
    }
}

/// Notes settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NotesConfig {
    /// Editor command for the note-editing flow.
    pub editor: String,
    /// Preview lines shown inline.
    pub preview_lines: u32,
    /// Whether notes are indexed for search.
    pub index: bool,
}

impl Default for NotesConfig {
    fn default() -> Self {
        Self {
            editor: "$EDITOR".to_owned(),
            preview_lines: 6,
            index: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// Compose/send/outbox settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SendConfig {
    /// Undo-send window (0 disables).
    pub undo_window: HumanDuration,
    /// Default timezone for scheduling.
    pub default_timezone: String,
    /// Outbox scheduler poll interval.
    pub poll_interval: HumanDuration,
    /// Grace period to still send a missed scheduled message.
    pub late_tolerance: HumanDuration,
    /// Maximum SMTP retries.
    pub max_retries: u32,
    /// Base retry backoff.
    pub backoff_base: HumanDuration,
    /// Maximum retry backoff.
    pub backoff_max: HumanDuration,
    /// Append sent mail to the IMAP Sent folder.
    pub append_to_sent: bool,
    /// MCP-originated sends always get an undo window.
    pub ai_requires_confirmation: bool,
    /// Optimal-send-time settings.
    pub optimal: SendOptimal,
    /// Follow-up tracker settings.
    pub followup: SendFollowup,
}

impl Default for SendConfig {
    fn default() -> Self {
        Self {
            undo_window: HumanDuration::new(secs(10)),
            default_timezone: "America/Los_Angeles".to_owned(),
            poll_interval: HumanDuration::new(secs(30)),
            late_tolerance: HumanDuration::new(mins(10)),
            max_retries: 5,
            backoff_base: HumanDuration::new(secs(30)),
            backoff_max: HumanDuration::new(mins(30)),
            append_to_sent: true,
            ai_requires_confirmation: true,
            optimal: SendOptimal::default(),
            followup: SendFollowup::default(),
        }
    }
}

/// Optimal-send-time settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SendOptimal {
    /// Whether optimal-time suggestion runs.
    pub enabled: bool,
    /// Suggestion model id.
    pub model: String,
    /// Earliest clock time (HH:MM).
    pub earliest: String,
    /// Latest clock time (HH:MM).
    pub latest: String,
    /// Prefer the recipient's timezone.
    pub prefer_recipient_tz: bool,
}

impl Default for SendOptimal {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "claude-haiku-4-5".to_owned(),
            earliest: "08:00".to_owned(),
            latest: "18:00".to_owned(),
            prefer_recipient_tz: true,
        }
    }
}

/// Follow-up tracker settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SendFollowup {
    /// Default delay before a follow-up nudge is due.
    pub default_delay: HumanDuration,
    /// Cancel the follow-up when a reply is detected.
    pub cancel_on_reply: bool,
}

impl Default for SendFollowup {
    fn default() -> Self {
        Self {
            default_delay: HumanDuration::new(days(3)),
            cancel_on_reply: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Finder
// ---------------------------------------------------------------------------

/// Fuzzy-finder settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FinderConfig {
    /// Whether the finder is enabled.
    pub enabled: bool,
    /// Default search scope.
    pub default_scope: FinderScope,
    /// Maximum results returned.
    pub max_results: u32,
    /// Maximum snippet length (bytes).
    pub snippet_max_bytes: u32,
    /// Dirty-feed drain interval (ms).
    pub refresh_interval_ms: u64,
    /// Smart-case matching.
    pub smart_case: bool,
    /// Show a preview pane.
    pub preview: bool,
    /// Blended-ranking weights.
    pub ranking: FinderRanking,
    /// Key bindings.
    pub keys: FinderKeys,
}

impl Default for FinderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_scope: FinderScope::All,
            max_results: 200,
            snippet_max_bytes: 160,
            refresh_interval_ms: 250,
            smart_case: true,
            preview: true,
            ranking: FinderRanking::default(),
            keys: FinderKeys::default(),
        }
    }
}

/// Finder blended-ranking weights.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FinderRanking {
    /// Recency half-life (days).
    pub half_life_days: u32,
    /// Recency weight.
    pub w_recency: f64,
    /// Unread weight.
    pub w_unread: f64,
    /// Importance weight.
    pub w_important: f64,
    /// Interaction-frequency weight.
    pub w_frequency: f64,
    /// Kind-prior weight.
    pub w_kind: f64,
}

impl Default for FinderRanking {
    fn default() -> Self {
        Self {
            half_life_days: 30,
            w_recency: 40.0,
            w_unread: 25.0,
            w_important: 30.0,
            w_frequency: 10.0,
            w_kind: 15.0,
        }
    }
}

/// Finder key bindings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FinderKeys {
    /// Open the finder.
    pub open: String,
    /// Open the command palette.
    pub commands: String,
    /// Toggle multi-select.
    pub multiselect: String,
}

impl Default for FinderKeys {
    fn default() -> Self {
        Self {
            open: "ctrl-p".to_owned(),
            commands: "ctrl-shift-p".to_owned(),
            multiselect: "tab".to_owned(),
        }
    }
}

// ---------------------------------------------------------------------------
// gRPC
// ---------------------------------------------------------------------------

/// gRPC server settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrpcConfig {
    /// Whether the gRPC server runs.
    pub enabled: bool,
    /// Unix domain socket path (`~` is expanded to `$HOME`).
    pub socket_path: String,
    /// Optional TCP listen address (empty disables TCP).
    pub listen: String,
    /// Whether TCP is enabled.
    pub tcp_enabled: bool,
    /// Authentication mode for TCP.
    pub auth: GrpcAuth,
    /// TLS settings.
    pub tls: GrpcTls,
    /// gRPC-web settings.
    pub web: GrpcWeb,
    /// Server limits.
    pub limits: GrpcLimits,
    /// Event-log retention.
    pub events: GrpcEvents,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            socket_path: "~/.local/state/rmail/rmaild.sock".to_owned(),
            listen: String::new(),
            tcp_enabled: false,
            auth: GrpcAuth::Token,
            tls: GrpcTls::default(),
            web: GrpcWeb::default(),
            limits: GrpcLimits::default(),
            events: GrpcEvents::default(),
        }
    }
}

impl GrpcConfig {
    /// The socket path with a leading `~` expanded to `$HOME`.
    #[must_use]
    pub fn resolved_socket_path(&self) -> PathBuf {
        expand_tilde(&self.socket_path)
    }
}

/// gRPC TLS settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrpcTls {
    /// Server certificate file.
    pub cert_file: String,
    /// Server key file.
    pub key_file: String,
    /// Client CA file (for mTLS).
    pub client_ca: String,
}

/// gRPC-web settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrpcWeb {
    /// Whether gRPC-web is enabled.
    pub enabled: bool,
    /// Allowed CORS origins.
    pub cors_origins: Vec<String>,
}

impl Default for GrpcWeb {
    fn default() -> Self {
        Self {
            enabled: false,
            cors_origins: vec!["http://localhost:5173".to_owned()],
        }
    }
}

/// gRPC server limits.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrpcLimits {
    /// Maximum message size (bytes).
    pub max_message_bytes: usize,
    /// Maximum concurrent streams.
    pub max_concurrent: u32,
    /// Per-stream buffer depth.
    pub stream_buffer: u32,
    /// Default request timeout (seconds).
    pub request_timeout_secs: u64,
}

impl Default for GrpcLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: 16_777_216,
            max_concurrent: 256,
            stream_buffer: 1024,
            request_timeout_secs: 120,
        }
    }
}

/// Event-log retention settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrpcEvents {
    /// Retention in days.
    pub retention_days: u32,
    /// Retention in rows.
    pub retention_rows: u64,
}

impl Default for GrpcEvents {
    fn default() -> Self {
        Self {
            retention_days: 7,
            retention_rows: 1_000_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const fn secs(n: u64) -> std::time::Duration {
    std::time::Duration::from_secs(n)
}
const fn mins(n: u64) -> std::time::Duration {
    std::time::Duration::from_secs(n * 60)
}
const fn days(n: u64) -> std::time::Duration {
    std::time::Duration::from_secs(n * 86_400)
}

/// Expand a leading `~` or `~/` to `$HOME`. Other paths are returned unchanged.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests;
