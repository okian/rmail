//! Natural-language smart folders: "a virtual mailbox defined by a
//! plain-English predicate that Claude compiles **once** into a stored hybrid
//! query, re-run cheaply on every sync" (prd.md feature 13; task 58).
//!
//! # Once, and then never again
//!
//! The provider call happens here, at create time, and nowhere else. What
//! reaches the database is a query string in rmail's own grammar plus a frozen
//! query embedding, and every subsequent evaluation — every sync, forever — is
//! [`super::membership`]: one SQL statement and one kNN against bytes already
//! on disk. A folder that phoned a model on each sync would be a per-message
//! recurring charge disguised as a saved search, which is the failure
//! [`crate::rules::synth`] spends a whole module preventing for rules.
//!
//! # The model proposes; three separate things commit
//!
//! ```text
//!   English ─▶ QueryCompiler ─▶ validate_hybrid_predicate ─▶ embed ─▶ create
//!               (Claude,          (the real parser;          (local)   (refuses
//!                cached)           unenforceable operators             an
//!                                  refused)                            unconstrained
//!                                                                      plan)
//! ```
//!
//! Nothing about that chain trusts the model's output as a *plan*. It is
//! re-parsed by [`crate::query::parse`], checked against the deterministic
//! membership compiler, and refused outright if it would hold the whole
//! account. A model that answers `larger:10mb` — a perfectly good search
//! operator the membership compiler cannot enforce — gets an
//! `INVALID_ARGUMENT`, not a folder that quietly contains everything.
//!
//! # The embedding is best-effort, and the folder is not
//!
//! `search.embedding_backend` can be unavailable (no local model file, a
//! hosted backend that is down), and a daemon configured with a wider model
//! than `vec_chunks` holds produces a vector this index cannot search at all.
//! The dense arm is then simply absent — prd.md's "Embeddings unavailable / no
//! key → dense retriever silently drops" applied to membership. What is *not*
//! best-effort is the folder itself: if the predicate's only enforceable arm
//! was going to be that embedding, [`super::SmartFolderStore::create`] refuses
//! to store it. A degraded arm is acceptable; a folder holding the account is
//! not.
//!
//! # A known rough edge: the two consumers accept different operators
//!
//! [`crate::query::compile`] is shared with `SearchService.CompileQuery`, and
//! its prompt teaches the model the *search* grammar — which is wider than the
//! membership grammar [`crate::tags::query`] can enforce. A sentence with a
//! date in it ("what did legal say last month?") compiles to `after:last-month
//! ...`, which `mail search --nl` runs happily and
//! [`super::validate_hybrid_predicate`] then refuses for a folder, because a
//! folder that silently ignored the date would hold strictly more than was
//! asked for.
//!
//! Failing closed is right; making it a routine outcome is not, and the real
//! fix is to teach [`crate::tags::query`] the date operators (it has no notion
//! of "now", which is the whole reason it does not back them). Until then the
//! rejection names the operator so the user can re-phrase, and the two paths
//! deliberately keep sharing one cache rather than diverging into two prompts
//! that would each pay for the same sentence.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::embed::{Embedder, Embedding};
use crate::error::Error;
use crate::query::compile::{CompiledQuery, QueryCompiler};
use crate::smart_folder::{NewSmartFolder, SmartFolder, SmartFolderStore};

/// What one natural-language folder definition produced.
#[derive(Debug, Clone, PartialEq)]
pub struct NlSmartFolder {
    /// The stored folder.
    pub folder: SmartFolder,
    /// The compiled plan it was defined by — what a client shows the user to
    /// confirm what was understood.
    pub compiled: CompiledQuery,
    /// Whether the dense arm made it in. `false` means the embedder was
    /// unavailable and membership rests on the filters and FTS alone, which
    /// is worth telling a user who asked for a semantic folder.
    pub semantic_arm: bool,
}

/// What [`NlSmartFolderCompiler::create`] needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewNlSmartFolder {
    /// Owning account.
    pub account_id: i64,
    /// Display name, unique per account.
    pub name: String,
    /// The plain-English predicate.
    pub description: String,
    /// A tag to apply to genuinely new members, if any.
    pub auto_tag: Option<String>,
    /// Whether a genuinely new member publishes an event.
    pub notify: bool,
    /// Recompile rather than serving the shared plan cache.
    pub refresh: bool,
}

/// Compiles English into smart folders.
///
/// Cheap to clone: every field is a handle.
#[derive(Debug, Clone)]
pub struct NlSmartFolderCompiler {
    compiler: QueryCompiler,
    embedder: Arc<dyn Embedder>,
    store: SmartFolderStore,
}

impl NlSmartFolderCompiler {
    /// Build a compiler over the shared query compiler, the configured query
    /// embedder, and the one smart folder store in the process.
    #[must_use]
    pub fn new(
        compiler: QueryCompiler,
        embedder: Arc<dyn Embedder>,
        store: SmartFolderStore,
    ) -> Self {
        Self {
            compiler,
            embedder,
            store,
        }
    }

    /// Compile `spec.description` and store the result as a smart folder.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for an empty/over-long description, a
    /// compiled predicate [`super::validate_hybrid_predicate`] refuses, or a
    /// plan with no enforceable constraint; [`Error::AlreadyExists`] if the
    /// account already has a folder by that name; whatever
    /// [`crate::ai::gate::admit`] returns when policy or a budget refuses the
    /// call; the provider's own error.
    #[tracing::instrument(
        skip(self, spec, cancel),
        fields(
            account_id = spec.account_id,
            name = spec.name,
            cached,
            semantic_arm,
        ),
        err
    )]
    pub async fn create(
        &self,
        spec: &NewNlSmartFolder,
        cancel: &CancellationToken,
    ) -> Result<NlSmartFolder, Error> {
        let compiled = self
            .compiler
            .compile(spec.account_id, &spec.description, spec.refresh, cancel)
            .await?;
        // Before anything is embedded or stored: the model's answer must be a
        // predicate this build can enforce in full.
        let predicate = super::validate_hybrid_predicate(&compiled.query)?;

        let vector = self.embed(&compiled.semantic_query).await;
        let semantic_arm = vector.is_some();
        let span = tracing::Span::current();
        span.record("cached", compiled.cached);
        span.record("semantic_arm", semantic_arm);

        let folder = self
            .store
            .create(&NewSmartFolder {
                account_id: spec.account_id,
                name: spec.name.clone(),
                predicate,
                auto_tag: spec.auto_tag.clone(),
                notify: spec.notify,
                nl_source: Some(compiled.raw.clone()),
                query_vector: vector,
                vector_model: semantic_arm.then(|| self.embedder.model().to_owned()),
                min_similarity: None,
                compiled_model: Some(compiled.model.clone()),
            })
            .await?;

        Ok(NlSmartFolder {
            folder,
            compiled,
            semantic_arm,
        })
    }

    /// Embed the free-text half, or `None` when there is none or the embedder
    /// could not answer.
    ///
    /// A failure is logged and degraded rather than propagated — see the
    /// module docs. It is deliberately not retried: the caller is a human
    /// waiting on a folder, and a folder that took thirty seconds to define
    /// because a hosted embedder was down is worse than one defined without
    /// its dense arm and told so.
    async fn embed(&self, text: &str) -> Option<Embedding> {
        if text.trim().is_empty() {
            return None;
        }
        match self.embedder.embed(&[text.to_owned()]).await {
            Ok(mut vectors) if !vectors.is_empty() => Some(vectors.remove(0)),
            Ok(_) => {
                tracing::warn!(
                    "the embedder returned no vector for this folder's free text; \
                     defining it without a semantic arm"
                );
                None
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not embed this folder's free text; defining it without a \
                     semantic arm"
                );
                None
            }
        }
    }
}
