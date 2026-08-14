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
    "hooks", "rules",
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

/// How SMTP submission is secured (task 61).
///
/// [`Self::Auto`] is the default and never resolves to [`Self::Plaintext`]:
/// downgrading a submission silently, on a heuristic, is how credentials end
/// up on the wire. An operator who genuinely relays through a local MTA on
/// port 25 says so by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// Implicit TLS on port 465, STARTTLS everywhere else.
    Auto,
    /// Always STARTTLS (the submission default, port 587).
    Starttls,
    /// TLS from the first byte (SMTPS, port 465).
    ImplicitTls,
    /// No TLS. For a trusted local relay only — everything, credentials
    /// included, travels in the clear.
    Plaintext,
}

impl SmtpSecurity {
    /// Resolve [`Self::Auto`] against the port actually configured.
    ///
    /// Returns a concrete variant, never `Auto`.
    #[must_use]
    pub fn resolve(self, port: u16) -> Self {
        match self {
            // 465 is SMTPS: TLS begins before the greeting, so STARTTLS
            // would hang waiting for a banner that is already encrypted.
            Self::Auto if port == 465 => Self::ImplicitTls,
            Self::Auto => Self::Starttls,
            other => other,
        }
    }
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
    /// Event-hook dispatcher settings.
    pub hooks: HooksConfig,
    /// Rules-engine settings.
    pub rules: RulesConfig,
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
    ///
    /// The master switch for task 64's local feedback log: `false` means
    /// `search_log`/`search_impression`/`search_action` are never written to
    /// at all (see [`crate::feedback`]), not that they are written and later
    /// ignored.
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
    /// Implicit-feedback log retention (task 64).
    pub feedback: FeedbackConfig,
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
            feedback: FeedbackConfig::default(),
        }
    }
}

/// How much of the implicit-feedback log is kept (task 64).
///
/// Both bounds apply and whichever bites first wins — the same shape
/// [`GrpcEvents`] uses, for the same reason: rows bound disk, age bounds how
/// stale the training corpus is allowed to get.
///
/// # Why this needs a bound at all
///
/// A `search_impression` row carries a *serialized* 34-feature vector
/// (`rmail_core::feedback::encode_features`), roughly 0.8 kB of JSON, and one
/// search logs up to a full page of them. Left unbounded that is the single
/// fastest-growing table in the database: at `search.default_limit = 25` and
/// the [`FeedbackConfig::max_queries`] default below, the worst case is on
/// the order of a hundred megabytes, and a real local corpus is a small
/// fraction of that (most pages are shorter, and nobody runs ten thousand
/// searches a quarter). The defaults are chosen so "leave learning on
/// forever" is a decision an operator can make without watching the disk.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FeedbackConfig {
    /// Age horizon, in days. `0` keeps nothing (a real answer, not a synonym
    /// for unlimited — see [`FeedbackConfig::max_queries`]).
    pub retention_days: u32,
    /// Hard ceiling on retained queries; the oldest are dropped first, taking
    /// their impressions and actions with them.
    ///
    /// There is deliberately no "unlimited" value for either bound. A config
    /// typo that silently disabled retention would grow the log without
    /// limit, which is the exact failure retention exists to prevent.
    pub max_queries: u64,
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            // A quarter: long enough that a seasonal query ("tax", "renewal")
            // appears more than once in the corpus, short enough that the
            // model is trained on what the mailbox looks like now.
            retention_days: 90,
            max_queries: 10_000,
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
    /// Where the cross-encoder's ONNX weights live.
    ///
    /// Empty means the same default the local embedder uses
    /// (`$RMAIL_MODEL_CACHE`, else `$XDG_CACHE_HOME/rmail/models`, else
    /// `~/.cache/rmail/models`) — one cache directory for every local model
    /// rather than a second one an operator has to provision separately.
    pub cross_encoder_cache_dir: String,
    /// Whether the daemon may fetch missing cross-encoder weights itself.
    ///
    /// Off by default, for the identical reason
    /// [`LocalEmbedConfig::allow_download`] is: the reranker's whole point is
    /// that mail text stays on the host, and a search that silently pulls
    /// several hundred megabytes from Hugging Face the first time somebody
    /// types would not honor that. An unprovisioned cache degrades to the L1
    /// order with a logged reason instead.
    pub cross_encoder_allow_download: bool,
    /// Claude rerank model id.
    pub claude_model: String,
    /// Max candidates sent to Claude for listwise rerank.
    pub claude_max_candidates: u32,
    /// Output-token ceiling for one listwise rerank turn. The answer is an
    /// ordering plus a one-line reason per candidate, so this scales with
    /// `claude_max_candidates` rather than with mailbox size.
    pub claude_max_tokens: u32,
    /// How many `(query, candidate_ids)` listwise verdicts stay cached in
    /// memory. Bounded because the key is content-addressed: nothing evicts
    /// an entry on its own, so an unbounded map would grow with distinct
    /// queries for the life of the daemon.
    pub claude_cache_entries: u32,
    /// Wall-clock ceiling for the whole L2 stage, whichever backend runs.
    /// Exceeding it degrades to the L1 order — prd.md's Stage 5 is an
    /// optional precision improvement, never a latency cliff.
    pub timeout: HumanDuration,
}

impl Default for RerankerConfig {
    fn default() -> Self {
        Self {
            cross_encoder_model: "bge-reranker-base".to_owned(),
            cross_encoder_cache_dir: String::new(),
            cross_encoder_allow_download: false,
            claude_model: "claude-haiku-4-5".to_owned(),
            claude_max_candidates: 30,
            claude_max_tokens: 4096,
            claude_cache_entries: 256,
            // prd.md budgets the local cross-encoder at <80 ms for 50 pairs
            // and the Claude path at "provider latency"; 20 s is a ceiling on
            // the pathological case (a cold ONNX session load, a slow
            // provider), not a target.
            timeout: HumanDuration::new(secs(20)),
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
    /// OCR image attachments and text-less PDFs (`Status::Empty` on native
    /// extraction) — opt-in and off by default, matching the PRD: OCR is CPU
    /// and, on macOS, framework-heavy, and a mailbox with a fast-clip
    /// scanner attached should not pay that cost until it asks to.
    /// Apple Vision is the backend on macOS (no operator setup — it ships
    /// with the OS); `tesseract` on `PATH` is the fallback, and the only
    /// option at all off macOS. See `attach::ocr`.
    pub ocr: bool,
    /// Languages to hint to the OCR backend, as tesseract-style ISO 639-2/T
    /// codes (`"eng"`) — Tesseract is passed these directly; Vision maps the
    /// common ones to the BCP-47 tags it expects and otherwise falls back to
    /// its own automatic language detection.
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
    /// Mailbox-RAG (`AskMailbox`) settings.
    pub ask: AiAsk,
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
    /// Prompt-injection shield settings.
    pub injection: AiInjection,
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
            ask: AiAsk::default(),
            limits: AiLimits::default(),
            batching: AiBatching::default(),
            prompt_cache: AiPromptCache::default(),
            retry: AiRetry::default(),
            privacy: AiPrivacy::default(),
            injection: AiInjection::default(),
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

/// Mailbox-RAG (`AiService.AskMailbox`, `mail ask`) settings.
///
/// Deliberately a small table. Everything that governs *whether* a call may
/// happen at all — spend caps, concurrency, redaction, per-folder
/// eligibility — already lives in `[ai.limits]`, `[ai.privacy]` and
/// `[ai.policy]`, and `ask` draws on those rather than restating them: a
/// second set of caps here would be a second answer to "may this call be
/// made", and the two would drift.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiAsk {
    /// The model that writes the grounded answer. prd.md's
    /// "`claude-sonnet-5` default for RAG/drafting" — not `ai.models.deep`,
    /// which is the *opus* tier a per-message deep pass uses.
    pub model: String,
    /// How many retrieved messages the context is built from, before the
    /// token budget cuts it further.
    pub top_k: u32,
    /// Ceiling on the assembled context, in estimated tokens. Packing stops
    /// at the first message that would cross it (see
    /// [`crate::ai::rag`]'s own docs on why it stops rather than skips).
    pub max_context_tokens: u32,
    /// How much of one message's body may enter the context. Bounded per
    /// message as well as in aggregate so a single enormous message cannot
    /// consume the entire budget and crowd out every other citation.
    pub max_chars_per_message: u32,
    /// Output-token ceiling for the answer.
    pub max_tokens: u32,
}

impl Default for AiAsk {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-5".to_owned(),
            top_k: 12,
            max_context_tokens: 8_000,
            max_chars_per_message: 2_000,
            max_tokens: 1_024,
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
    /// Per-call budget enforcement (soft-cap model downgrade, hard-cap block,
    /// the bulk sub-budget). Nested under `limits` rather than given its own
    /// `[ai.budget]` table because it enforces *these* caps at a finer grain:
    /// `daily_cost_cap_usd`/`daily_token_cap`/`monthly_cost_cap_usd` above are
    /// where the global budget's hard ceilings come from when no operator
    /// override has been stored — see [`AiBudget`].
    pub budget: AiBudget,
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
            budget: AiBudget::default(),
        }
    }
}

/// Budget-enforcer settings — see [`crate::ai::budget`] for what each knob
/// actually decides.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiBudget {
    /// Whether the per-call budget enforcer runs at all. Turning it off does
    /// **not** turn off `ai.limits`' cycle-level cost gate
    /// ([`crate::ai::queue::CostGate`]) — that is a separate, coarser control
    /// that still pauses dispatch when the day's global spend is exhausted.
    pub enabled: bool,
    /// Where the soft cap sits, as a fraction of the hard cap, for any cap a
    /// stored budget row leaves unset. `0.8` means "start downgrading the
    /// model once 80% of the ceiling is gone." A value outside `0.0..1.0`
    /// disables the derived soft cap entirely (an explicit soft cap set via
    /// `SetBudget` still applies).
    pub soft_cap_ratio: f64,
    /// The share of a scope's hard caps that bulk work may consume, for any
    /// scope with no explicit `bulk` budget row. `0.5` means a backlog walk
    /// can spend at most half the day's budget, leaving the rest for
    /// interactive and triage work no matter how much backlog there is.
    pub bulk_share: f64,
    /// Queue priority at or beyond which a job is charged as bulk work.
    /// Defaults to [`crate::ai::queue::PRIORITY_BACKFILL`] — a backlog walk
    /// is bulk; `PRIORITY_NORMAL`/`PRIORITY_RECENT` work is not.
    pub bulk_priority: i64,
    /// The model ids a soft-cap downgrade steps between.
    pub ladder: AiModelLadder,
}

impl Default for AiBudget {
    fn default() -> Self {
        Self {
            enabled: true,
            soft_cap_ratio: 0.8,
            bulk_share: 0.5,
            bulk_priority: 500,
            ladder: AiModelLadder::default(),
        }
    }
}

/// The `opus → sonnet → haiku` ladder a soft cap steps down.
///
/// Every default here is a model id [`crate::ai::estimate_cost_usd`]'s pricing
/// table knows. That is load-bearing, not incidental: a downgrade to a model
/// the ledger cannot price would record `cost_usd = 0.0` for every call made
/// after the soft cap engaged, so crossing the soft cap would make the *hard*
/// cap unreachable — the budget would stop counting exactly when it matters
/// most. An operator retargeting this ladder at a newer model id must add it
/// to that table in the same change.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiModelLadder {
    /// Top rung.
    pub opus: String,
    /// Middle rung.
    pub sonnet: String,
    /// Bottom rung; nothing steps below it.
    pub haiku: String,
}

impl Default for AiModelLadder {
    fn default() -> Self {
        Self {
            opus: "claude-opus-4-8".to_owned(),
            sonnet: "claude-sonnet-5".to_owned(),
            haiku: "claude-haiku-4-5".to_owned(),
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

/// Prompt-injection shield settings — see
/// [`crate::ai::injection`]'s module docs.
///
/// Neither knob here can turn off the *structural* half of the shield.
/// Untrusted mail is fenced and labelled as data on every path that shows it
/// to a model, unconditionally, because a switch that could put
/// attacker-authored text back into instruction position would be a switch
/// labelled "be exploitable". What is configurable is the second control:
/// whether the pattern detector runs at all, and how much it has to find
/// before a model-decided *action* is withheld pending confirmation.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AiInjection {
    /// Whether the injection detector runs and records what it finds.
    ///
    /// Turning this off necessarily disables the action gate too — a gate
    /// with nothing to gate on cannot fail closed — so it is an explicit,
    /// documented opt-out for an operator who has decided their mail source
    /// is trusted, not a performance knob.
    pub enabled: bool,
    /// The lowest severity that withholds an AI-decided action pending
    /// confirmation: `hostile`, `suspicious`, or `never`.
    ///
    /// `hostile` (the default) covers text addressed to the model — an
    /// instruction override, forged system/tool framing, an exfiltration
    /// request. `suspicious` additionally covers obfuscation on its own
    /// (zero-width characters, homoglyphs, CSS-hidden text), which real
    /// marketing mail carries often enough that it is not the default: a
    /// gate that fires on half a mailbox is a gate an operator turns off.
    pub block_actions_at: String,
}

impl Default for AiInjection {
    fn default() -> Self {
        Self {
            enabled: true,
            block_actions_at: "hostile".to_owned(),
        }
    }
}

impl AiInjection {
    /// [`Self::block_actions_at`] parsed, or `None` for `never` **and** for
    /// an unrecognized value.
    ///
    /// Folding "off" and "typo" into the same answer is deliberate, and it
    /// is the opposite of what [`crate::ai::deep`]'s `priority_at_least`
    /// does with its own unvalidated operator string. The asymmetry is about
    /// which direction the mistake runs: an unrecognized *spend* threshold
    /// that failed open would let cost escape, so that one fails closed;
    /// an unrecognized threshold here that failed closed would silently
    /// withhold every AI-decided rule action in the mailbox, with the only
    /// evidence being actions that quietly stop happening. A misconfigured
    /// shield that warns loudly and keeps the mailbox working is the better
    /// failure — and it does warn: [`Self::warn_if_unrecognized`] is called
    /// once at startup rather than on every evaluation.
    #[must_use]
    pub fn block_at(&self) -> Option<crate::ai::injection::Severity> {
        crate::ai::injection::Severity::parse(&self.block_actions_at)
    }

    /// Warn, once, if [`Self::block_actions_at`] is neither a severity nor
    /// the literal `never` — the startup-time counterpart to
    /// [`Self::block_at`]'s deliberate fail-open.
    pub fn warn_if_unrecognized(&self) {
        if self.block_at().is_none() && self.block_actions_at != "never" {
            tracing::warn!(
                block_actions_at = %self.block_actions_at,
                recognized = ?["hostile", "suspicious", "never"],
                "ai.injection.block_actions_at is not a recognized value; no AI-decided rule \
                 action will be withheld for a suspected prompt injection until this is fixed"
            );
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
// Hooks
// ---------------------------------------------------------------------------

/// Event-hook dispatcher settings (prd.md #48 "Event Hook Dispatcher"):
/// config-driven shell commands that fire on mail events, run in a bounded
/// worker pool with a per-hook timeout. See `rmail_core::hooks`'s own module
/// docs for the dispatcher itself — in particular, why the event JSON only
/// ever reaches a hook on stdin, never interpolated into the command.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HooksConfig {
    /// Whether the hook dispatcher's background consumer runs at all.
    ///
    /// A disabled dispatcher still registers `HookService` — reflection and
    /// the auth scope table see every RPC regardless of runtime config, the
    /// same convention `AiService`/`ai.enabled` already established (see
    /// `rmaild::serve_uds_with_engine_and_mail_store`'s own comment) — and
    /// `TestHook` still runs on demand: it is an operator-invoked dry run,
    /// not "did the automatic dispatcher fire," so it stays available even
    /// with the automatic path off.
    pub enabled: bool,
    /// Bounded worker pool size: the maximum number of hook processes
    /// running concurrently across every configured hook. `0` is coerced up
    /// to `1` by `hooks::HookDispatcher::new` — a config typo here degrades
    /// the dispatcher to fully serial rather than silently running nothing
    /// at all.
    pub max_concurrency: u32,
    /// Default per-hook execution timeout, overridable per hook
    /// (`[[hooks.hooks]] timeout = "..."`).
    pub default_timeout: HumanDuration,
    /// How often the dispatch loop re-reads the event log.
    ///
    /// This is the *upper bound on hook latency*: an event appended just
    /// after a tick waits nearly a full interval before its hooks fire (see
    /// the `hooks` module docs on why this polls rather than holding a live
    /// subscription open). Lowering it trades a query per tick against that
    /// latency; raising it is reasonable for hooks that only do bookkeeping.
    pub tick_interval: HumanDuration,
    /// Maximum bytes of stdout/stderr retained per run. Output past this
    /// cap is still drained (never left to back up the pipe, which would
    /// stall the hook and, transitively, the dispatcher — see the `hooks`
    /// module docs), just not kept.
    pub max_output_bytes: u32,
    /// Configured hooks.
    pub hooks: Vec<HookConfig>,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrency: 4,
            default_timeout: HumanDuration::new(secs(30)),
            tick_interval: HumanDuration::new(crate::hooks::DEFAULT_TICK_INTERVAL),
            max_output_bytes: 64 * 1024,
            hooks: Vec::new(),
        }
    }
}

/// One configured hook: a shell command bound to a mail event.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// Unique, human-chosen name — what `ListHooks`/`TestHook` and
    /// `mail hook add`/`mail hook test` address it by. A duplicate name is
    /// not rejected by this type (TOML has no native way to reject a
    /// duplicate key across array-of-table elements); `hooks::resolve`
    /// drops later duplicates with a logged warning instead — see that
    /// function's own docs.
    pub name: String,
    /// Which mail event fires this hook.
    pub event: HookEvent,
    /// The program to execute — resolved via `$PATH`, exactly like
    /// `std::process::Command::new`; never a shell. An operator who wants
    /// shell features (pipes, redirects, globbing) names `/bin/sh` here
    /// with `args = ["-c", "..."]`, the same convention cron/systemd use —
    /// this field itself performs no shell interpretation, which is what
    /// keeps the event JSON's stdin-only contract airtight (see the
    /// `hooks` module docs).
    pub command: String,
    /// Additional argv entries, in order, after `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Whether this hook is active. A disabled hook is still listed by
    /// `ListHooks` and runnable via `TestHook` (an operator validating a
    /// hook before flipping it on), but the dispatcher never fires it from
    /// a real event.
    #[serde(default = "default_hook_enabled")]
    pub enabled: bool,
    /// Per-hook timeout override. Falls back to `hooks.default_timeout`
    /// when unset.
    #[serde(default)]
    pub timeout: Option<HumanDuration>,
}

const fn default_hook_enabled() -> bool {
    true
}

/// The fixed vocabulary of mail events a hook can subscribe to (prd.md #48).
///
/// Deliberately its own type rather than a reuse of `events::EventKind`: a
/// hook's vocabulary is the *product* surface named in the PRD/proto
/// (`on_new_message`, `on_label`, ...), while `EventKind` is the durable
/// event bus's internal wire vocabulary — and the two are not 1:1.
/// `OnSyncError` in particular names no distinct `EventKind` at all; it is
/// `EventKind::SyncState` filtered to entries whose payload carries a
/// non-null `error` (see `hooks::hook_matches` for exactly that filter).
/// Collapsing the two enums into one would either invent an `EventKind` no
/// other subscriber publishes, or leak dispatcher-internal filtering into
/// the wire/config contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// A new message synced (`events::EventKind::NewMail`).
    OnNewMessage,
    /// A message's flags/labels changed (`events::EventKind::FlagChanged`).
    OnLabel,
    /// A message moved between folders (`events::EventKind::Moved`).
    OnMove,
    /// A rule matched and acted (`events::EventKind::RuleFired`).
    OnRuleMatch,
    /// A sync pass recorded an error (`events::EventKind::SyncState` whose
    /// payload's `error` field is non-null).
    OnSyncError,
}

// ---------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------

/// Rules-engine settings (task 66, prd.md #45/#46/#50).
///
/// The rules themselves are **not** here. Unlike `[[hooks.hooks]]`, a rule is
/// per account, created and backtested over gRPC, and stored in the database
/// — see `rmail_core::rules`'s own module docs and migration V35 for why. This
/// table holds only the knobs that govern how the engine runs them.
///
/// There is deliberately no model knob either: a `claude_is` classification is
/// exactly the cheap, high-volume work `ai.models.triage` names, and synthesis
/// is the one-off reasoning job `ai.models.deep` names. A second place to
/// configure a model is a second place for it to drift out of step with the
/// budget ladder that prices those two.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RulesConfig {
    /// Whether the background evaluator runs at all.
    ///
    /// A disabled evaluator still registers `RuleService` — reflection and
    /// the auth scope table see every RPC regardless of runtime config, the
    /// convention `AiService`/`HookService` established — so creating,
    /// listing, backtesting, and explicitly evaluating rules all still work.
    /// What this gates is only the automatic "on each new message" path.
    pub enabled: bool,
    /// How often the evaluator re-reads the event log. This is the upper
    /// bound on how long after a message arrives its rules fire.
    pub tick_interval: HumanDuration,
    /// How many messages one tick evaluates before deferring the rest.
    pub max_batch: u32,
    /// The mailbox an `archive = true` action moves to.
    pub archive_mailbox: String,
    /// Maximum length, in bytes, of one predicate's regex source.
    ///
    /// This and the two limits below bound *untrusted* patterns — a rule's
    /// regexes come from a user, or from a model, and are then run unattended
    /// against every new message. See `rmail_core::rules::model`'s own docs
    /// for what each one stops and why a timeout is not among them.
    pub max_pattern_len: u32,
    /// Maximum size, in bytes, of one compiled regex program. A pattern that
    /// would exceed it is refused when the rule is created, not when it is
    /// first matched.
    pub regex_size_limit_bytes: u32,
    /// Maximum characters of any one field a regex is matched against.
    pub max_match_chars: u32,
    /// How many user corrections are replayed as few-shot examples on an
    /// uncached `claude_is` call. Every example is tokens on every such call.
    pub max_examples: u32,
    /// How many messages a backtest or synthesis dry run examines. A backtest
    /// is an interactive question; one over a whole mailbox's history is a
    /// different (batch) feature.
    pub max_window_messages: u32,
    /// Default window, in days, for a synthesis dry run.
    pub dry_run_days: u32,
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tick_interval: HumanDuration::new(crate::rules::DEFAULT_TICK_INTERVAL),
            max_batch: 200,
            archive_mailbox: "Archive".to_owned(),
            max_pattern_len: 512,
            regex_size_limit_bytes: 256 * 1024,
            max_match_chars: 64 * 1024,
            max_examples: 8,
            max_window_messages: 500,
            dry_run_days: 30,
        }
    }
}

impl RulesConfig {
    /// The pattern bounds these settings describe.
    #[must_use]
    pub fn rule_limits(&self) -> crate::rules::RuleLimits {
        crate::rules::RuleLimits {
            // Floored like the two below, and for the same reason: a
            // `max_pattern_len = 0` typo would refuse every pattern including
            // `a`, with an error about byte counts that names nothing an
            // operator would connect to the knob they set.
            max_pattern_len: (self.max_pattern_len as usize).max(64),
            // Floored, not passed through: a `regex_size_limit_bytes = 0`
            // typo would refuse every pattern including `a`, turning a
            // misconfiguration into "no rule in this daemon works" with an
            // error message about program size that names nothing an operator
            // would connect to the knob they set.
            regex_size_limit_bytes: (self.regex_size_limit_bytes as usize).max(4 * 1024),
            // Floored for the mirror-image reason: a zero here would truncate
            // every haystack to nothing, so every regex silently stops
            // matching rather than failing loudly.
            max_match_chars: (self.max_match_chars as usize).max(1_024),
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
    ///
    /// Setting this `false` shortens that window to
    /// [`crate::outbox::MIN_AI_UNDO_WINDOW`]; it cannot remove it. See
    /// `rmail_core::outbox::policy`'s module docs for why that floor is not
    /// negotiable.
    pub ai_requires_confirmation: bool,
    /// Bounded SMTP worker pool: how many messages may be in flight at once.
    ///
    /// prd.md's default is 2. `0` is coerced up to `1` by
    /// [`crate::outbox::SendPolicy::from_config`] — a typo here should make
    /// the daemon slow, not silently stop sending mail.
    pub workers: u32,
    /// How SMTP submission is secured. See [`SmtpSecurity`].
    pub smtp_security: SmtpSecurity,
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
            workers: 2,
            smtp_security: SmtpSecurity::Auto,
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
