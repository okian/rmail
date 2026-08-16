//! `QueryPlan` assembly: turning task 25's operator-parsed [`ParsedQuery`]
//! into the fully-understood query Stage 1's retrievers consume (prd.md,
//! "Stage 0 — Query Understanding").
//!
//! # What lands here versus what stays in `parse`
//!
//! `parse.rs` is deliberately narrow: operator grammar only, no ranking, no
//! I/O, no corpus knowledge. Everything Stage 0 does that *needs* the corpus
//! — spelling correction against what this mailbox actually contains, contact
//! resolution against the contact graph, synonym expansion from local term
//! co-occurrence, embedding the query, classifying intent — is this module's
//! job, and it is why [`QueryPlanner`] holds a [`Database`] and an embedder
//! while [`parse::parse`] holds neither.
//!
//! Task 25 also left every date operator's value as a raw string
//! (`before:last-week`, `date:2025-06..2025-08`) rather than resolving it,
//! because doing so needs a notion of "now" and a date grammar that has
//! nothing to do with the operator grammar itself. Resolving those is this
//! module's job too — see [`HardFilter`].
//!
//! # Deterministic, and no Claude
//!
//! Every field on [`QueryPlan`] is computed from this mailbox's own data: the
//! FTS5 vocabulary (spell-fix, synonym co-occurrence), the contact graph
//! (alias resolution), the configured embedder (the dense vector), and a
//! hand-tuned feature scorer (intent). "Configured" is doing real work in
//! that sentence — [`QueryPlanner`] takes whatever [`Embedder`] the caller
//! built from `search.embedding_backend`, local by default but a hosted
//! provider (Voyage) if configured, exactly like every other consumer of the
//! `Embedder` trait. What this module guarantees is narrower and absolute:
//! it never calls **Claude**, or anything in [`crate::ai`]. prd.md's Stage 0
//! step 7 ("NL → plan (Claude, cached)") and the local co-occurrence synonym
//! path's optional Claude variant (`search.expansion.claude`) both stay out
//! of this module. The one seam this module leaves for them is
//! [`QueryPlan::needs_nl_compile`]: a cheap, local, deterministic signal that
//! the query reads like prose an operator grammar can't structure. A later
//! task (43/58) reads that flag and may replace or augment the plan with a
//! Claude-compiled one; until it exists, a prose query still gets *this*
//! plan — weaker, carrying only what the embedding and any recognized
//! entities buy it — rather than nothing.
//!
//! # Corpus vocabulary, without a second index
//!
//! Spell-fix and synonym expansion both need to know what words actually
//! appear in this mailbox and how often. Rather than maintain a parallel
//! vocabulary table that could drift from the lexical index, migration V16
//! creates `fts_messages_vocab`, an `fts5vocab` shadow table SQLite derives
//! live from `fts_messages` (task 18's index). It has no population step and
//! cannot go stale independently of the index it mirrors.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Months, NaiveDate, Utc, Weekday};
use rusqlite::{named_params, params, OptionalExtension};
use unicode_normalization::UnicodeNormalization;

use crate::config::ExpansionConfig;
use crate::embed::{Embedder, Embedding};
use crate::error::Error;
use crate::index::entities::{self, EntityKind};
use crate::storage::Database;

use super::parse::{self, Filter, Mode, Operator, ParsedQuery, Phrase};

// ---------------------------------------------------------------------------
// QueryPlan and its parts
// ---------------------------------------------------------------------------

/// The fully-assembled Stage 0 output: everything Stage 1's retrievers and
/// Stage 2's fusion need, computed once per query rather than re-derived by
/// every retriever.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryPlan {
    /// The original, unmodified input — carried through from
    /// [`ParsedQuery::raw`] for cache keys and `--explain` output.
    pub raw: String,
    /// Every operator [`parse::parse`] recognized. A date-shaped one
    /// (`before:`/`after:`/`on:`/`date:`) carries the absolute range this
    /// stage resolved it to — see [`HardFilter`] for why that is a distinct
    /// variant rather than an optional field.
    pub hard_filters: Vec<HardFilter>,
    /// Free-text terms, ranked rather than filtered: the original from
    /// [`ParsedQuery::terms`], plus a spell-corrected sibling for any term
    /// [`QueryPlanner`] found a confident correction for.
    pub lexical_terms: Vec<PlanTerm>,
    /// Quoted phrases, passed through from [`ParsedQuery::phrases`]
    /// unchanged — spell-fix and expansion operate on single words, not
    /// exact-match spans.
    pub phrases: Vec<Phrase>,
    /// Synonyms pulled from this mailbox's own term co-occurrence (PMI), soft
    /// and down-weighted — evidence for recall, never a claim about what the
    /// user asked for.
    pub expansions: Vec<PlanTerm>,
    /// The query's free text, embedded once by the local embedder, for the
    /// dense retriever's kNN. `None` when there is no free text to embed (a
    /// filters-only query) or the embedder itself failed — degrading to a
    /// lexical-only plan rather than failing the whole query, per prd.md's
    /// "Graceful degradation" principle.
    pub query_vector: Option<Embedding>,
    /// Resolved people and entity-shaped spans: contacts matched from free
    /// text (soft `from:`/`to:` boosts, never a hard filter — see
    /// [`EntityRefKind::Contact`]) and entity patterns recognized in the
    /// query text itself (task 19's extractors, reused verbatim).
    pub entities: Vec<EntityRef>,
    /// The classified search intent — shifts Stage 2's fusion weights.
    pub intent: Intent,
    /// Result ordering. Always [`SortSpec::Relevance`] out of this stage: the
    /// operator grammar has no `sort:` operator, so an explicit sort is a
    /// caller preference (CLI `--sort`, `SearchRequest`) applied after
    /// planning, not something query text implies.
    pub sort: SortSpec,
    /// Account/mailbox scope narrowed by `account:`/`in:` filters. Empty
    /// means "everything configured" — the caller's default, not a
    /// constraint this stage invented.
    pub scope: Scope,
    /// Whether this query looks like prose a deterministic operator grammar
    /// could not structure, and would benefit from Claude compiling it into a
    /// plan (prd.md's Stage 0 step 7). See the module docs — this module
    /// never acts on the flag itself.
    pub needs_nl_compile: bool,
}

/// One operator, as parsed, with a date-shaped one's absolute range folded
/// in — or dropped, if it could not be resolved.
///
/// This started as a struct — `{ filter: Filter, resolved_date:
/// Option<DateRange> }` — and that shape had a footgun: a
/// `Before`/`After`/`On`/`DateRange` operator whose value did not resolve
/// (`before:whenever`) still carried its raw string in `filter`, right next
/// to a `resolved_date: None` a careless reader could miss. A retriever that
/// matched on `Operator::Before(_)` without checking `resolved_date` first
/// would silently re-derive (or ignore) the raw string and half-enforce
/// exactly the constraint [`resolve_filters`] decided it couldn't — the
/// failure mode this whole module exists to prevent. Under this enum, an
/// unresolved date filter is indistinguishable from any other non-date
/// operator ([`HardFilter::Other`]) at the type level: there is no field
/// left to misuse.
#[derive(Debug, Clone, PartialEq)]
pub enum HardFilter {
    /// A `before:`/`after:`/`on:`/`date:` operator that resolved to an
    /// absolute range. `filter` is kept — raw value and negation — for
    /// `--explain`/re-parse; `range` is what a retriever filters on.
    Date {
        /// The operator and its negation, exactly as [`parse::parse`]
        /// produced it.
        filter: Filter,
        /// The resolved absolute range.
        range: DateRange,
    },
    /// Every other operator, verbatim — including a date-shaped one whose
    /// value did not resolve, or whose range was inverted
    /// (`date:2025-08..2025-06`). Dropped as an enforceable constraint, the
    /// same degrade-gracefully rule `parse.rs` applies to a value that
    /// doesn't fit its operator's shape.
    Other(Filter),
}

impl HardFilter {
    /// The underlying operator, however it resolved — for a caller that only
    /// needs `--explain`/re-parse and does not care whether it is a date.
    #[must_use]
    pub fn filter(&self) -> &Filter {
        match self {
            Self::Date { filter, .. } | Self::Other(filter) => filter,
        }
    }
}

/// An absolute, half-open UTC range in unix seconds: `[start, end)`. Either
/// bound may be absent — `before:`/`after:` constrain only one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DateRange {
    /// Inclusive lower bound, unix seconds. `None` means unbounded below.
    pub start: Option<i64>,
    /// Exclusive upper bound, unix seconds. `None` means unbounded above.
    pub end: Option<i64>,
}

/// A free-text term as retrieval sees it: [`parse::Term`]'s negation and
/// mode, plus the relevance weight and provenance this stage assigns.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanTerm {
    /// The word (corrected, for a [`TermOrigin::SpellFixed`] entry; the
    /// synonym itself, for [`TermOrigin::Synonym`]).
    pub text: String,
    /// `true` if the *original* term carried a leading `-`.
    pub negated: bool,
    /// The retrieval mode requested by a `~`/`=` prefix on the original term.
    /// Always [`Mode::Auto`] for a [`TermOrigin::Synonym`] entry — an
    /// inferred expansion has no sigil of its own to inherit.
    pub mode: Mode,
    /// Relevance weight relative to an unmodified term's `1.0`: above for a
    /// spell correction ("corrected boosted", prd.md Stage 0 step 3), below
    /// for a synonym expansion ("soft, down-weighted", step 5).
    pub weight: f64,
    /// Where this term came from.
    pub origin: TermOrigin,
}

/// Why a [`PlanTerm`] exists.
#[derive(Debug, Clone, PartialEq)]
pub enum TermOrigin {
    /// Exactly what the user typed, negation/mode already applied.
    Original,
    /// A spelling correction of another term in the same plan. `from` is the
    /// original spelling, kept so `--explain` can show "did you mean...".
    SpellFixed {
        /// The term text this correction was derived from.
        from: String,
    },
    /// A synonym surfaced by local PMI co-occurrence. `from` is the pivot
    /// term it co-occurs with.
    Synonym {
        /// The term text this synonym was derived from.
        from: String,
    },
}

/// A resolved person or entity-shaped span, carried as a soft ranking signal
/// — never a hard filter, per prd.md Stage 0 step 4 ("added as soft
/// `from:`/`to:` boosts, not hard filters unless the user typed the
/// operator").
#[derive(Debug, Clone, PartialEq)]
pub struct EntityRef {
    /// What kind of thing this is and, for a contact, nothing further; for a
    /// pattern, which [`EntityKind`] matched.
    pub kind: EntityRefKind,
    /// Canonical form used to match the entity/contact graph (a lowercased
    /// email address, an IBAN's compact form, ...).
    pub norm: String,
    /// Display form, when it differs from `norm` — a contact's name.
    pub display: Option<String>,
    /// The free-text span in `raw` that resolved to this entity, for
    /// `--explain` and highlighting.
    pub source_text: String,
    /// Soft ranking boost. Never gates a candidate out — see the type docs.
    pub boost: f64,
}

/// What an [`EntityRef`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityRefKind {
    /// A contact resolved from a name or address fragment in free text that
    /// the user did *not* write as a `from:`/`to:`/`cc:` operator — see
    /// [`EntityRef`]'s docs on why this stays soft.
    Contact,
    /// An entity-shaped span recognized in the query text itself (an email,
    /// IBAN, amount, tracking/order/invoice number, ...) by task 19's
    /// deterministic extractors, reused rather than reimplemented.
    Pattern(EntityKind),
}

/// The classified search intent (prd.md, Stage 0 step 2). Shifts Stage 2's
/// per-source fusion weights (`search.fusion_weights`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Intent {
    /// "the invoice Acme sent last week" — a known item; favor exact match,
    /// recency, and sender affinity, in a tight result set.
    Navigational,
    /// "everything about the office move" — a broad topic; favor semantic
    /// recall and diversity.
    Exploratory,
    /// "tracking number for my order", "AWS bill" — a structured fact; favor
    /// the entity index and structured filters.
    Lookup,
}

/// Result ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortSpec {
    /// Ranked by the fused/reranked score (the default).
    #[default]
    Relevance,
    /// Newest first.
    Date,
    /// Grouped/ordered by sender.
    Sender,
}

/// Account/mailbox scope narrowed by `account:`/`in:` filters.
///
/// A projection, not a second source of truth: every `account:`/`in:`
/// operator it summarizes also stays in [`QueryPlan::hard_filters`]
/// (`Scope::from_filters` only reads them, never removes them). `hard_filters`
/// is authoritative for *matching* — it is what carries negation, and what a
/// retriever should evaluate as `WHERE` predicates; `Scope` exists so a
/// caller that just needs "which account(s)/mailbox(es) to route this query
/// to" (index selection, connection routing) does not have to scan
/// `hard_filters` for two specific operators to get it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scope {
    /// Account names from non-negated `account:` filters. Empty means every
    /// configured account.
    pub accounts: Vec<String>,
    /// Mailbox/folder names from non-negated `in:` filters. Empty means every
    /// mailbox within `accounts`.
    pub mailboxes: Vec<String>,
}

impl Scope {
    /// Project the operators that narrow scope out of a filter list. A
    /// negated `-in:Spam` stays represented in `hard_filters` as an
    /// exclusion; it does not belong here, which is a *positive* statement of
    /// where to look.
    fn from_filters(filters: &[Filter]) -> Self {
        let mut accounts = Vec::new();
        let mut mailboxes = Vec::new();
        for filter in filters {
            if filter.negated {
                continue;
            }
            match &filter.op {
                Operator::Account(name) => accounts.push(name.clone()),
                Operator::In(name) => mailboxes.push(name.clone()),
                _ => {}
            }
        }
        Self {
            accounts,
            mailboxes,
        }
    }
}

impl PlanTerm {
    fn original(term: &parse::Term) -> Self {
        Self {
            text: term.text.clone(),
            negated: term.negated,
            mode: term.mode,
            weight: ORIGINAL_WEIGHT,
            origin: TermOrigin::Original,
        }
    }

    fn spell_fixed(corrected: &str, term: &parse::Term) -> Self {
        Self {
            text: corrected.to_owned(),
            negated: term.negated,
            mode: term.mode,
            weight: CORRECTED_WEIGHT,
            origin: TermOrigin::SpellFixed {
                from: term.text.clone(),
            },
        }
    }

    fn synonym(text: &str, from: &str) -> Self {
        Self {
            text: text.to_owned(),
            negated: false,
            mode: Mode::Auto,
            weight: EXPANSION_WEIGHT,
            origin: TermOrigin::Synonym {
                from: from.to_owned(),
            },
        }
    }
}

/// One term's resolved surface form, alongside the retrieval mode its sigil
/// requested — the intermediate value between "what the user typed" and
/// "what gets embedded/expanded". Not part of [`QueryPlan`]'s public shape;
/// [`PlanTerm`] is.
struct ResolvedTerm {
    /// Corrected text if spell-fix found one, the original otherwise.
    text: String,
    /// The mode a `~`/`=` prefix on the *original* term requested. Carried
    /// through so [`QueryPlanner::expand_synonyms`] can honor parse.rs's
    /// sigil contract — see its doc comment.
    mode: Mode,
}

/// Weight of an unmodified free-text term — the datum every other
/// [`PlanTerm::weight`] is relative to.
const ORIGINAL_WEIGHT: f64 = 1.0;

/// Weight of a spell-corrected term. Above [`ORIGINAL_WEIGHT`], not instead
/// of it: prd.md says "original and corrected terms both retrieved; corrected
/// boosted", so a bad correction still leaves the original term searchable at
/// its normal weight.
const CORRECTED_WEIGHT: f64 = 1.2;

/// Weight of a PMI-expanded synonym. Below [`ORIGINAL_WEIGHT`] — prd.md calls
/// expansions "soft, down-weighted": a synonym is recall evidence, not intent
/// the user stated, and must never outrank what they actually typed.
const EXPANSION_WEIGHT: f64 = 0.35;

// ---------------------------------------------------------------------------
// QueryPlanner
// ---------------------------------------------------------------------------

/// Assembles [`QueryPlan`]s. Holds what plan assembly needs beyond the raw
/// query text: the corpus (spell-fix, synonyms, contacts) and the query
/// embedder.
///
/// Cheap to clone: `db` shares a connection pool and `embedder` is already
/// behind an `Arc`, the same pattern [`crate::index::fts::FtsIndex`] and
/// [`crate::index::semantic::SemanticIndex`] use.
#[derive(Clone)]
pub struct QueryPlanner {
    db: Database,
    embedder: Arc<dyn Embedder>,
    expansion: ExpansionConfig,
}

impl std::fmt::Debug for QueryPlanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryPlanner")
            .field("model", &self.embedder.model())
            .field("spellfix", &self.expansion.spellfix)
            .field("synonyms", &self.expansion.synonyms)
            .finish()
    }
}

impl QueryPlanner {
    /// Build a planner over a database and the configured query embedder.
    #[must_use]
    pub fn new(db: Database, embedder: Arc<dyn Embedder>, expansion: ExpansionConfig) -> Self {
        Self {
            db,
            embedder,
            expansion,
        }
    }

    /// Assemble a [`QueryPlan`] for `raw`, resolving any relative date
    /// expressions against the current moment.
    ///
    /// # Errors
    ///
    /// A mapped storage error from any of the corpus lookups. Embedding
    /// failures do not propagate — see [`QueryPlan::query_vector`].
    pub async fn plan(&self, raw: &str) -> Result<QueryPlan, Error> {
        self.plan_at(raw, Utc::now()).await
    }

    /// As [`QueryPlanner::plan`], with an injected "now" so relative dates
    /// (`before:last-week`) resolve reproducibly — the pipeline is
    /// deterministic *given* a moment, not independent of one.
    ///
    /// # Errors
    ///
    /// See [`QueryPlanner::plan`].
    #[tracing::instrument(
        skip(self, raw),
        fields(
            hard_filters,
            lexical_terms,
            expansions,
            entities,
            query_vector,
            intent,
            needs_nl_compile,
        )
    )]
    pub async fn plan_at(&self, raw: &str, now: DateTime<Utc>) -> Result<QueryPlan, Error> {
        // `raw` is deliberately excluded from the span (see `skip` above): a
        // mail search query is user content, and every other span field
        // below is a shape/count a trace can carry without ever quoting what
        // was typed.
        let parsed = parse::parse(raw);

        let hard_filters = resolve_filters(&parsed.filters, now);
        let (lexical_terms, resolved_terms) = self.spellfix_terms(&parsed.terms).await?;
        let entities = self.resolve_entities(&parsed).await?;
        let expansions = if self.expansion.synonyms {
            self.expand_synonyms(&resolved_terms).await?
        } else {
            Vec::new()
        };
        let query_vector = self.embed_query(&resolved_terms, &parsed.phrases).await;
        let intent = classify_intent(&parsed, &entities);
        let scope = Scope::from_filters(&parsed.filters);
        // Prose the deterministic grammar found nothing to structure in:
        // no operator pinned any part of it, no phrase quoted an exact
        // span, and it opens like a sentence rather than a keyword list.
        let needs_nl_compile = parsed.filters.is_empty()
            && parsed.phrases.is_empty()
            && starts_with_question_word(&parsed.raw);

        let plan = QueryPlan {
            raw: parsed.raw,
            hard_filters,
            lexical_terms,
            phrases: parsed.phrases,
            expansions,
            query_vector,
            entities,
            intent,
            sort: SortSpec::default(),
            scope,
            needs_nl_compile,
        };

        let span = tracing::Span::current();
        span.record("hard_filters", plan.hard_filters.len());
        span.record("lexical_terms", plan.lexical_terms.len());
        span.record("expansions", plan.expansions.len());
        span.record("entities", plan.entities.len());
        span.record("query_vector", plan.query_vector.is_some());
        span.record("intent", tracing::field::debug(plan.intent));
        span.record("needs_nl_compile", plan.needs_nl_compile);
        tracing::debug!("query plan assembled");

        Ok(plan)
    }

    /// Original terms, plus a spell-corrected sibling for any eligible term
    /// that isn't already in the corpus vocabulary. Also returns each
    /// non-negated term's resolved form ([`ResolvedTerm`]: corrected text if
    /// one was found, original otherwise, plus its mode) for callers —
    /// synonym expansion and embedding — that want one surface form per term
    /// rather than the original/corrected pair.
    async fn spellfix_terms(
        &self,
        terms: &[parse::Term],
    ) -> Result<(Vec<PlanTerm>, Vec<ResolvedTerm>), Error> {
        let mut out = Vec::with_capacity(terms.len());
        let mut resolved = Vec::with_capacity(terms.len());
        for term in terms {
            out.push(PlanTerm::original(term));
            let mut chosen = term.text.clone();
            // A negated term is an exclusion, not a search word — correcting
            // it would change what gets excluded, which is not what
            // "spell-fix the query" means. A `~`/`=` term is excluded for a
            // different reason: parse.rs's module docs say `=` "bypasses
            // semantic recall and query expansion" — spell-fix adds a second
            // term the user did not type, which is exactly query expansion,
            // so only an unmoded (`Mode::Auto`) term is ever corrected.
            if self.expansion.spellfix
                && !term.negated
                && term.mode == Mode::Auto
                && is_plain_word(&term.text)
            {
                if let Some(correction) = self.spellfix_candidate(&term.text).await? {
                    out.push(PlanTerm::spell_fixed(&correction, term));
                    chosen = correction;
                }
            }
            if !term.negated {
                resolved.push(ResolvedTerm {
                    text: chosen,
                    mode: term.mode,
                });
            }
        }
        Ok((out, resolved))
    }

    /// The best correction for `raw_term` against the corpus vocabulary, or
    /// `None` if it is already a known word or nothing within its edit
    /// budget is.
    async fn spellfix_candidate(&self, raw_term: &str) -> Result<Option<String>, Error> {
        // The FTS5 tokenizer (`unicode61 remove_diacritics 2` in V9) both
        // case-folds and diacritic-folds every stored term before it ever
        // reaches `fts_messages_vocab` — "café" is indexed as "cafe". Folding
        // the query term the same way here is what makes vocabulary
        // membership actually mean what indexing already assumes; skipping
        // it would make a correctly-typed "café" permanently "correctable"
        // to "cafe", since the two would never compare equal.
        let lower = fold_diacritics(raw_term).to_lowercase();
        let len = lower.chars().count();
        let budget = edit_budget(len);
        if budget == 0 {
            return Ok(None);
        }
        let min_len = i64::try_from(len.saturating_sub(budget)).unwrap_or(0);
        let max_len = i64::try_from(len.saturating_add(budget)).unwrap_or(i64::MAX);
        Ok(self
            .db
            .read(move |conn| {
                let exact: Option<i64> = conn
                    .query_row(
                        "SELECT cnt FROM fts_messages_vocab WHERE term = ?1",
                        [&lower],
                        |row| row.get(0),
                    )
                    .optional()?;
                if exact.is_some() {
                    return Ok(None);
                }
                // `fts5vocab` has no index on `length(term)`/`doc` — every
                // predicate here is evaluated per row, not pushed down — so
                // `LIMIT` is what keeps a mailbox with a million-token
                // vocabulary from materializing all of it into this `Vec` on
                // every misspelled word. It bounds the worst case rather than
                // avoiding the scan entirely; a real SymSpell delete-index
                // would be the fix if this ever shows up in a profile.
                let mut stmt = conn.prepare(
                    "SELECT term, doc FROM fts_messages_vocab
                     WHERE length(term) BETWEEN ?1 AND ?2 AND doc >= ?3
                     LIMIT ?4",
                )?;
                let rows = stmt
                    .query_map(
                        params![
                            min_len,
                            max_len,
                            MIN_CANDIDATE_DOC_FREQ,
                            SPELLFIX_CANDIDATE_LIMIT
                        ],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )?
                    .collect::<rusqlite::Result<Vec<(String, i64)>>>()?;

                // Closest edit distance wins; a tie prefers the more common
                // word, then alphabetical for a stable result across runs.
                let mut best: Option<(String, usize, i64)> = None;
                for (candidate, doc_freq) in rows {
                    let distance = edit_distance(&lower, &candidate);
                    if distance == 0 || distance > budget {
                        continue;
                    }
                    let better = match &best {
                        None => true,
                        Some((best_term, best_dist, best_doc)) => {
                            distance < *best_dist
                                || (distance == *best_dist && doc_freq > *best_doc)
                                || (distance == *best_dist
                                    && doc_freq == *best_doc
                                    && candidate < *best_term)
                        }
                    };
                    if better {
                        best = Some((candidate, distance, doc_freq));
                    }
                }
                Ok(best.map(|(term, _, _)| term))
            })
            .await?)
    }

    /// Synonyms of `pivots` (the query's best-known term forms) surfaced by
    /// local PMI co-occurrence, excluding anything the user already typed.
    ///
    /// A `~`/`=`-moded term never pivots — parse.rs's module docs: `=`
    /// "bypasses semantic recall and query expansion", and a PMI synonym is
    /// exactly that, an expansion the user did not ask for on that specific
    /// term. It can still be excluded *as a candidate* below: if the user
    /// typed it, offering it again as someone else's synonym is redundant
    /// regardless of its own mode.
    async fn expand_synonyms(&self, pivots: &[ResolvedTerm]) -> Result<Vec<PlanTerm>, Error> {
        let literal: BTreeSet<String> = pivots.iter().map(|p| p.text.to_lowercase()).collect();
        let mut out = Vec::new();
        let mut queried: BTreeSet<String> = BTreeSet::new();
        let mut emitted: BTreeSet<String> = BTreeSet::new();
        for pivot in pivots {
            if pivot.mode != Mode::Auto {
                continue;
            }
            let lower = fold_diacritics(&pivot.text).to_lowercase();
            if !is_plain_word(&lower) || !queried.insert(lower.clone()) {
                continue;
            }
            for (candidate, _pmi) in self.pmi_candidates(&lower).await? {
                // Not a *new* recall signal if the user already typed this
                // word — "invoice receipt" does not need "receipt" added
                // again as a down-weighted synonym of "invoice".
                if literal.contains(&candidate) || !emitted.insert(candidate.clone()) {
                    continue;
                }
                out.push(PlanTerm::synonym(&candidate, &pivot.text));
            }
        }
        Ok(out)
    }

    /// Candidate synonyms of `pivot` and their PMI score, best first, capped
    /// at [`MAX_SYNONYMS_PER_TERM`].
    ///
    /// PMI(pivot, candidate) = ln( P(candidate | pivot) / P(candidate) ),
    /// estimated from a bounded sample of the messages containing `pivot`
    /// rather than the whole corpus: a personal mailbox's vocabulary is a
    /// few thousand terms, not a web-scale one, so a sample this small still
    /// gives a stable estimate, and bounding it is what keeps this a
    /// per-query cost rather than a corpus scan.
    async fn pmi_candidates(&self, pivot: &str) -> Result<Vec<(String, f64)>, Error> {
        let pivot = pivot.to_owned();
        Ok(self
            .db
            .read(move |conn| {
                let total_docs: i64 =
                    conn.query_row("SELECT count(*) FROM fts_messages", [], |row| row.get(0))?;
                if total_docs == 0 {
                    return Ok(Vec::new());
                }
                let pivot_doc_freq: Option<i64> = conn
                    .query_row(
                        "SELECT doc FROM fts_messages_vocab WHERE term = ?1",
                        [&pivot],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(pivot_doc_freq) = pivot_doc_freq else {
                    // Not in the corpus at all — nothing to learn a
                    // co-occurrence from. A misspelled pivot reaches here
                    // only when spell-fix is off or found no correction.
                    return Ok(Vec::new());
                };
                if pivot_doc_freq < MIN_PIVOT_DOC_FREQ
                    || (pivot_doc_freq as f64) > (total_docs as f64) * MAX_PIVOT_DOC_FRACTION
                {
                    // Too rare to have a stable co-occurrence signal, or so
                    // common (a stopword-like term) that "what co-occurs
                    // with it" is close to "everything".
                    return Ok(Vec::new());
                }

                let sample_ids: Vec<i64> = {
                    let mut stmt = conn.prepare(
                        "SELECT rowid FROM fts_messages WHERE fts_messages MATCH ?1
                         ORDER BY rowid DESC LIMIT ?2",
                    )?;
                    // Quoted as an FTS5 string literal (internal `"`
                    // doubled) so the pivot is matched as a literal token,
                    // never re-interpreted as FTS5 query syntax. Newest
                    // first: `rowid` is `messages.id`, an insertion order
                    // that tracks arrival, so a sample this small is biased
                    // toward the mailbox's current vocabulary rather than
                    // whatever the oldest 64 matches happened to be about.
                    let query = format!("\"{}\"", pivot.replace('"', "\"\""));
                    let ids = stmt
                        .query_map(params![query, PMI_SAMPLE_LIMIT], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<i64>>>()?;
                    ids
                };
                if (sample_ids.len() as i64) < MIN_COOCCUR_DOCS {
                    return Ok(Vec::new());
                }

                let mut co_doc: BTreeMap<String, i64> = BTreeMap::new();
                let mut budget = PMI_SCAN_BYTE_BUDGET;
                // `scanned` counts documents this pass actually merged into
                // `co_doc`, which is *not* always `sample_ids.len()`: the
                // byte budget can end the scan early. Using the requested
                // sample size as `P(candidate | pivot)`'s denominator once
                // the budget has truncated it would understate every
                // co-occurrence by the same factor — on an ordinary mailbox
                // of quoted-thread bodies, easily enough to sink every
                // candidate below `PMI_THRESHOLD` and silently turn
                // expansion off. The denominator has to be what was actually
                // counted.
                let mut scanned: i64 = 0;
                let mut text_stmt = conn.prepare_cached(
                    "SELECT text FROM index_content
                     WHERE message_id = ?1 AND part IN ('subject', 'body')",
                )?;
                'sample: for id in &sample_ids {
                    let texts: Vec<String> = text_stmt
                        .query_map([id], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<String>>>()?;
                    let mut doc_terms: BTreeSet<String> = BTreeSet::new();
                    for text in &texts {
                        let Some(remaining) = budget.checked_sub(text.len()) else {
                            // The sample-wide budget is spent. This document
                            // is dropped whole, not partially counted — a
                            // half-scanned document would under-report its
                            // own terms relative to every fully-scanned one
                            // — and no later document is scanned either, so
                            // `scanned` stops exactly where the budget did.
                            break 'sample;
                        };
                        budget = remaining;
                        doc_terms.extend(tokenize(text));
                    }
                    scanned += 1;
                    for term in doc_terms {
                        if term != pivot {
                            *co_doc.entry(term).or_insert(0) += 1;
                        }
                    }
                }
                if scanned < MIN_COOCCUR_DOCS {
                    return Ok(Vec::new());
                }
                if scanned < sample_ids.len() as i64 {
                    tracing::debug!(
                        pivot = %pivot,
                        requested = sample_ids.len(),
                        scanned,
                        "PMI scan budget cut the sample short"
                    );
                }
                let sample_size = scanned;

                let mut doc_freq_stmt =
                    conn.prepare_cached("SELECT doc FROM fts_messages_vocab WHERE term = ?1")?;
                let mut scored = Vec::new();
                for (candidate, co_count) in co_doc {
                    if co_count < MIN_COOCCUR_DOCS || !is_plain_word(&candidate) {
                        continue;
                    }
                    let cand_doc_freq: Option<i64> = doc_freq_stmt
                        .query_row([&candidate], |row| row.get(0))
                        .optional()?;
                    let Some(cand_doc_freq) = cand_doc_freq else {
                        continue;
                    };
                    if (cand_doc_freq as f64) > (total_docs as f64) * MAX_PIVOT_DOC_FRACTION {
                        continue;
                    }
                    let p_candidate_given_pivot = co_count as f64 / sample_size as f64;
                    let p_candidate = cand_doc_freq as f64 / total_docs as f64;
                    if p_candidate <= 0.0 {
                        continue;
                    }
                    let pmi = (p_candidate_given_pivot / p_candidate).ln();
                    if pmi >= PMI_THRESHOLD {
                        scored.push((candidate, pmi));
                    }
                }
                scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                scored.truncate(MAX_SYNONYMS_PER_TERM);
                Ok(scored)
            })
            .await?)
    }

    /// Entity-shaped spans in the query text, plus contacts resolved from
    /// free text that wasn't already pinned to an operator.
    async fn resolve_entities(&self, parsed: &ParsedQuery) -> Result<Vec<EntityRef>, Error> {
        // Pattern-shaped entities are a pure scan over the raw text — no DB,
        // reusing task 19's extractors exactly as they run over mail body
        // text. Scanning the whole raw string (rather than excluding
        // operator syntax) can double-count a literal date/email that also
        // appears inside an operator value; that is redundant evidence, not
        // an incorrect one, so it is left rather than special-cased away.
        let mut out: Vec<EntityRef> = entities::scan(&parsed.raw)
            .into_iter()
            .map(|mention| EntityRef {
                kind: EntityRefKind::Pattern(mention.kind),
                norm: mention.norm,
                display: None,
                source_text: mention.value,
                boost: PATTERN_BOOST_SCALE * mention.confidence,
            })
            .collect();

        let mut candidates: Vec<String> = Vec::new();
        for term in &parsed.terms {
            // `looked_like_operator` terms already tried and failed to be an
            // operator value — resolving them as a contact too would be
            // guessing at a second reading of the same token.
            if term.negated || term.looked_like_operator {
                continue;
            }
            if term.text.chars().count() >= MIN_CONTACT_TERM_LEN && !is_stopword(&term.text) {
                candidates.push(term.text.to_lowercase());
            }
        }
        for phrase in &parsed.phrases {
            if phrase.negated {
                continue;
            }
            if phrase.text.chars().count() >= MIN_CONTACT_TERM_LEN {
                candidates.push(phrase.text.to_lowercase());
            }
        }

        if !candidates.is_empty() {
            out.extend(self.match_contacts(candidates).await?);
        }

        let mut out = dedup_entities(out);
        // Strongest signal first, so a query with more entities than
        // `MAX_ENTITY_REFS` loses its weakest evidence rather than
        // whichever kind happened to be pushed first — patterns were always
        // built before contacts, so insertion-order truncation silently
        // favored patterns regardless of which mattered more.
        out.sort_by(|a, b| b.boost.total_cmp(&a.boost));
        out.truncate(MAX_ENTITY_REFS);
        Ok(out)
    }

    /// Contacts whose name or address matches a free-text candidate,
    /// strongest (most-exchanged-with) first.
    ///
    /// One substring pattern covers a first name ("bob" in "Bob Jones"), a
    /// local part ("bob" in "bob@example.com"), a domain fragment ("acme" in
    /// "billing@acme.com"), and the full address typed verbatim — all are
    /// just substrings of `name`/`address`. A `LIKE '%...%'` scan of
    /// `contacts` is fine at personal-mailbox scale (hundreds to low
    /// thousands of rows); a larger deployment would want a trigram or
    /// prefix index instead of a full scan per candidate.
    ///
    /// No `lower()` on the SQL side: SQLite's `LIKE` is already
    /// case-insensitive for ASCII without it (the `case_sensitive_like`
    /// pragma defaults off), and `lower()` is *ASCII-only* folding —
    /// wrapping `name`/`address` in it while the candidate arrives already
    /// fully Unicode-lowercased in Rust (`resolve_entities`) combined two
    /// different case-folding rules and could disagree on an accented
    /// capital (`JOSÉ`'s SQL `lower()` leaves `É` alone; Rust's does not).
    /// Comparing both sides through the same rule — SQLite's native ASCII
    /// fold — is narrower (a fully Unicode-aware match needs an ICU
    /// extension this build doesn't have) but at least consistent.
    async fn match_contacts(&self, candidates: Vec<String>) -> Result<Vec<EntityRef>, Error> {
        Ok(self
            .db
            .read(move |conn| {
                let mut out = Vec::new();
                // `:pattern` is bound once and matched twice: a named
                // parameter, unlike a plain `?`, can appear more than once in
                // the SQL text while still taking a single value.
                let mut stmt = conn.prepare_cached(
                    "SELECT address, name, message_count FROM contacts
                     WHERE name LIKE :pattern ESCAPE '\\'
                        OR address LIKE :pattern ESCAPE '\\'
                     ORDER BY message_count DESC LIMIT :limit",
                )?;
                for candidate in &candidates {
                    let pattern = format!("%{}%", escape_like(candidate));
                    let rows = stmt.query_map(
                        named_params! {
                            ":pattern": pattern,
                            ":limit": MAX_CONTACT_MATCHES_PER_TERM,
                        },
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        },
                    )?;
                    for row in rows {
                        let (address, name, message_count) = row?;
                        out.push(EntityRef {
                            kind: EntityRefKind::Contact,
                            norm: address.to_lowercase(),
                            display: name,
                            source_text: candidate.clone(),
                            boost: contact_boost(message_count),
                        });
                    }
                }
                Ok(out)
            })
            .await?)
    }

    /// Embed the query's free text once, for the dense retriever.
    ///
    /// `None` when there is nothing to embed, or when the embedder itself
    /// fails — prd.md's "Graceful degradation" principle: an absent or
    /// broken embedder must weaken a plan, not fail it, so lexical/fuzzy/
    /// entity recall can still carry the query alone.
    async fn embed_query(&self, terms: &[ResolvedTerm], phrases: &[Phrase]) -> Option<Embedding> {
        let mut parts: Vec<&str> = terms.iter().map(|t| t.text.as_str()).collect();
        parts.extend(
            phrases
                .iter()
                .filter(|phrase| !phrase.negated)
                .map(|phrase| phrase.text.as_str()),
        );
        let joined = parts.join(" ");
        if joined.trim().is_empty() {
            return None;
        }
        match self.embedder.embed(&[joined]).await {
            Ok(mut vectors) => vectors.pop(),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "query embedding failed; continuing without a dense vector"
                );
                None
            }
        }
    }
}

/// Minimum documents a candidate must appear in before spell-fix will offer
/// it, so a single OCR artifact or one-off typo in the corpus itself can
/// never become someone else's "correction".
const MIN_CANDIDATE_DOC_FREQ: i64 = 2;

/// Largest number of vocabulary rows one spell-fix candidate scan reads.
/// `fts5vocab` has no index to push `length(term)`/`doc` into, so this bounds
/// the worst case (a mailbox whose vocabulary runs to the millions) rather
/// than the typical one — see the call site.
const SPELLFIX_CANDIDATE_LIMIT: i64 = 4000;

/// Messages sampled per PMI pivot term. Bounded so expansion stays a
/// per-query cost proportional to a constant, not to how often the pivot
/// occurs in a large mailbox.
const PMI_SAMPLE_LIMIT: i64 = 64;

/// A pivot must occur in at least this many messages before its
/// co-occurrence is trusted at all.
const MIN_PIVOT_DOC_FREQ: i64 = 2;

/// A pivot/candidate pair must co-occur in at least this many sampled
/// messages — two coincidental appearances together is noise, not a pattern.
const MIN_COOCCUR_DOCS: i64 = 2;

/// A pivot or candidate occurring in more than this fraction of the corpus is
/// treated as stopword-like: "what co-occurs with 'the'" is not a synonym
/// relationship, it is the whole mailbox.
const MAX_PIVOT_DOC_FRACTION: f64 = 0.6;

/// Total message-text bytes one PMI expansion may scan, across its whole
/// sample. Bounds cost the same way [`crate::index::entities`]'s
/// `MAX_MESSAGE_SCAN_BYTES` bounds entity extraction — a query must not pay
/// for an unbounded number of enormous message bodies.
const PMI_SCAN_BYTE_BUDGET: usize = 512 * 1024;

/// Minimum PMI for a candidate to be offered as a synonym: `ln(x) >= 1.0`
/// means the pair co-occurs at least ~e≈2.7× more often than chance predicts.
const PMI_THRESHOLD: f64 = 1.0;

/// Synonyms kept per pivot term, best PMI first.
const MAX_SYNONYMS_PER_TERM: usize = 3;

/// Shortest word spell-fix or synonym expansion will pivot on. Below this,
/// almost any word is a plausible "correction" of any other, and precision
/// collapses.
const MIN_WORD_LEN: usize = 3;

/// Entity refs kept per plan (patterns plus contacts, combined).
const MAX_ENTITY_REFS: usize = 32;

/// Contact rows matched per free-text candidate.
const MAX_CONTACT_MATCHES_PER_TERM: i64 = 5;

/// Shortest free-text term/phrase worth trying against the contact graph.
const MIN_CONTACT_TERM_LEN: usize = 3;

/// Base multiplier applied to a pattern-matched entity's own confidence to
/// get its ranking boost — kept below 1.0 because, like every [`EntityRef`],
/// this is a soft signal, not a filter.
const PATTERN_BOOST_SCALE: f64 = 0.6;

/// Ceiling on a contact-match boost.
const CONTACT_BOOST_MAX: f64 = 0.5;

/// Message count at which a contact's boost saturates — beyond this many
/// exchanged messages, more history doesn't make the match any more certain.
const CONTACT_BOOST_SATURATE: i64 = 20;

/// A contact's ranking boost, scaled by how much mail has actually been
/// exchanged with them — frequent correspondents deserve a stronger nudge,
/// capped so one hyperactive contact can't dominate every query that matches
/// their name.
fn contact_boost(message_count: i64) -> f64 {
    let saturated = message_count.clamp(0, CONTACT_BOOST_SATURATE) as f64;
    CONTACT_BOOST_MAX * (saturated / CONTACT_BOOST_SATURATE as f64)
}

/// Collapse entity refs that resolved to the same underlying thing. A
/// literal email typed in the query (`EntityRefKind::Pattern(EntityKind::
/// Email)`) and a contact matched by name or address
/// (`EntityRefKind::Contact`) can both normalize to the same address, and a
/// retriever that sums boosts across `entities` must not count that
/// agreement twice.
///
/// `norm` alone is the identity, not `(kind, norm)`: a `Contact` and a
/// `Pattern(Email)` for the same address are exactly the case this exists to
/// merge, and no other pair of kinds can collide by accident — an amount's
/// norm (`"USD 1299.00"`), a date's (`"2024-03-01"`), an IBAN's (compact
/// uppercase), a tracking number's — none of those shapes is ever also a
/// lowercased email address.
///
/// A `Contact` wins over a `Pattern` regardless of which carries the larger
/// `boost`: it carries a display name and a message-history-driven boost, so
/// it is strictly more informative than "this text is shaped like an email",
/// and a saturated contact's `CONTACT_BOOST_MAX` can be numerically smaller
/// than a bare pattern's — comparing boosts alone would silently discard the
/// display name on exactly the frequent correspondents it matters most for.
/// Within the same kind, the higher boost wins; a tie keeps whichever was
/// seen first.
fn dedup_entities(entities: Vec<EntityRef>) -> Vec<EntityRef> {
    let mut out: Vec<EntityRef> = Vec::with_capacity(entities.len());
    for entity in entities {
        let Some(existing) = out.iter_mut().find(|existing| existing.norm == entity.norm) else {
            out.push(entity);
            continue;
        };
        let contact_over_pattern =
            entity.kind == EntityRefKind::Contact && existing.kind != EntityRefKind::Contact;
        let better_within_kind = entity.kind == existing.kind && entity.boost > existing.boost;
        if contact_over_pattern || better_within_kind {
            *existing = entity;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Date resolution
// ---------------------------------------------------------------------------

/// Resolve every date-shaped filter's raw value into an absolute range.
/// Every other filter passes through unchanged — [`parse::parse`] already
/// decided its shape; this is only the part it deliberately left undone.
///
/// `before:`/`after:` both anchor on the *start* of the resolved span, not
/// its end: `before:2026-07-01` is strictly before that day begins, and
/// `after:2026-07-01` is on or after it — matching `on:` docs' "date exactly
/// matching" and `after:` docs' "on or after". Applied to a period rather
/// than a single day (`after:last-week`), the same rule reads as "from the
/// start of last week onward", which composes the same way for any
/// precision the grammar accepts.
pub(crate) fn resolve_filters(filters: &[Filter], now: DateTime<Utc>) -> Vec<HardFilter> {
    filters
        .iter()
        .map(|filter| match &filter.op {
            Operator::Before(raw) => match resolve_date_span(raw, now) {
                Some((start, _)) => HardFilter::Date {
                    filter: filter.clone(),
                    range: DateRange {
                        start: None,
                        end: Some(start.timestamp()),
                    },
                },
                None => HardFilter::Other(filter.clone()),
            },
            Operator::After(raw) => match resolve_date_span(raw, now) {
                Some((start, _)) => HardFilter::Date {
                    filter: filter.clone(),
                    range: DateRange {
                        start: Some(start.timestamp()),
                        end: None,
                    },
                },
                None => HardFilter::Other(filter.clone()),
            },
            Operator::On(raw) => match resolve_date_span(raw, now) {
                Some((start, end)) => HardFilter::Date {
                    filter: filter.clone(),
                    range: DateRange {
                        start: Some(start.timestamp()),
                        end: Some(end.timestamp()),
                    },
                },
                None => HardFilter::Other(filter.clone()),
            },
            Operator::DateRange(from, to) => {
                match (resolve_date_span(from, now), resolve_date_span(to, now)) {
                    // Strictly less-than, not `<=`: a same-day range
                    // (`date:2025-06-01..2025-06-01`) has `start` at that
                    // day's beginning and `end` at the *next* day's
                    // beginning (the end bound is always the end of its own
                    // span, never its start), so it never trips this check.
                    // Only a genuinely inverted range does.
                    (Some((start, _)), Some((_, end))) if start < end => HardFilter::Date {
                        filter: filter.clone(),
                        range: DateRange {
                            start: Some(start.timestamp()),
                            end: Some(end.timestamp()),
                        },
                    },
                    // One side did not resolve, or the range is inverted
                    // (`date:2025-08..2025-06`): a half-enforced or
                    // backwards range would silently constrain the query to
                    // something the input never asked for. Dropping the
                    // constraint (see `HardFilter`'s docs) is the honest
                    // degrade.
                    _ => HardFilter::Other(filter.clone()),
                }
            }
            _ => HardFilter::Other(filter.clone()),
        })
        .collect()
}

/// Resolve one date expression — as it appeared in a `before:`/`after:`/
/// `on:`/`date:` value — into the half-open UTC span it denotes.
///
/// `now` is a parameter rather than a call to [`Utc::now`] so "last week" is
/// reproducible in a test — see [`QueryPlanner::plan_at`].
fn resolve_date_span(expr: &str, now: DateTime<Utc>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let trimmed = expr.trim();
    resolve_absolute_date(trimmed).or_else(|| resolve_relative_date(trimmed, now))
}

/// `YYYY-MM-DD`, `YYYY-MM`, or `YYYY` — the precisions the grammar's own
/// examples use (`date:2025-06..2025-08` ranges by month). A coarser
/// precision resolves to the whole period it names, so a month bound expands
/// to cover every day in it.
fn resolve_absolute_date(text: &str) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return day_span(date);
    }
    if !text.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
        return None;
    }
    match text.split_once('-') {
        Some((year, month)) if year.len() == 4 && month.len() == 2 => {
            month_span(year.parse().ok()?, month.parse().ok()?)
        }
        Some(_) => None,
        None if text.len() == 4 => year_span(text.parse().ok()?),
        None => None,
    }
}

/// Named relative dates, resolved against `now`'s calendar day in UTC.
///
/// A small, fixed vocabulary rather than a general natural-language date
/// parser: `before:`/`after:`/`on:` are part of the deterministic operator
/// grammar, not the NL-compile path ([`QueryPlan::needs_nl_compile`]), so
/// what they accept has to stay enumerable and testable. `_`/space fold into
/// `-` so `last-week`/`last_week` (and a quoted `"last week"` operator value)
/// all resolve the same way.
fn resolve_relative_date(text: &str, now: DateTime<Utc>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let today = now.date_naive();
    let key: String = text
        .to_lowercase()
        .chars()
        .map(|c| if c == '_' || c == ' ' { '-' } else { c })
        .collect();
    // `checked_*` throughout, not `+`/`-`: `now` reaches here from the public
    // `plan_at(raw, now)`, so a date near chrono's representable range is a
    // caller input this function must decline gracefully, not a bug it can
    // assume away.
    match key.as_str() {
        "today" => day_span(today),
        "yesterday" => day_span(today.checked_sub_signed(Duration::days(1))?),
        "this-week" => week_span(today),
        "last-week" => week_span(today.checked_sub_signed(Duration::weeks(1))?),
        "this-month" => month_span(today.year(), today.month()),
        "last-month" => {
            let (year, month) = previous_month(today.year(), today.month())?;
            month_span(year, month)
        }
        "this-year" => year_span(today.year()),
        "last-year" => year_span(today.year().checked_sub(1)?),
        _ => None,
    }
}

fn day_span(day: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start = day.and_hms_opt(0, 0, 0)?.and_utc();
    let end = start.checked_add_signed(Duration::days(1))?;
    Some((start, end))
}

fn week_span(any_day_in_week: NaiveDate) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let week = any_day_in_week.week(Weekday::Mon);
    let (start, _) = day_span(week.checked_first_day()?)?;
    let (_, end) = day_span(week.checked_last_day()?)?;
    Some((start, end))
}

fn month_span(year: i32, month: u32) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start_day = NaiveDate::from_ymd_opt(year, month, 1)?;
    let end_day = start_day.checked_add_months(Months::new(1))?;
    let (start, _) = day_span(start_day)?;
    let (end, _) = day_span(end_day)?;
    Some((start, end))
}

fn year_span(year: i32) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let start_day = NaiveDate::from_ymd_opt(year, 1, 1)?;
    let end_day = NaiveDate::from_ymd_opt(year.checked_add(1)?, 1, 1)?;
    let (start, _) = day_span(start_day)?;
    let (end, _) = day_span(end_day)?;
    Some((start, end))
}

fn previous_month(year: i32, month: u32) -> Option<(i32, u32)> {
    if month == 1 {
        Some((year.checked_sub(1)?, 12))
    } else {
        Some((year, month - 1))
    }
}

// ---------------------------------------------------------------------------
// Intent classification
// ---------------------------------------------------------------------------

/// Cold-start intent classifier: a hand-tuned linear (one-vs-rest logistic)
/// scorer over cheap query features, in the same spirit as Stage 4's
/// deterministic cold-start ranker (prd.md's `RankWeights`) — a reasoned
/// starting point a trained model replaces once there is feedback to learn
/// from (prd.md, "Personalization & Implicit-Feedback Learning Loop"). There
/// is no such feedback yet, so "cheap local feature logistic" here means
/// weights chosen to match the PRD's own worked examples (see this module's
/// tests), not fit to data.
fn classify_intent(parsed: &ParsedQuery, entities: &[EntityRef]) -> Intent {
    let term_count = parsed.terms.len() + parsed.phrases.len();
    let has_operator = !parsed.filters.is_empty();
    let has_phrase = !parsed.phrases.is_empty();
    let has_question_word = starts_with_question_word(&parsed.raw);
    let has_known_contact = entities.iter().any(|e| e.kind == EntityRefKind::Contact);
    let has_entity_pattern = entities
        .iter()
        .any(|e| matches!(e.kind, EntityRefKind::Pattern(_)));
    let has_date_signal = parsed.filters.iter().any(|f| {
        matches!(
            f.op,
            Operator::Before(_) | Operator::After(_) | Operator::On(_) | Operator::DateRange(_, _)
        )
    }) || contains_relative_date_phrase(&parsed.raw);

    let denom = term_count.max(1) as f64;
    let lookup_frac = lexicon_fraction(&parsed.terms, LOOKUP_LEXICON, denom);
    let exploratory_frac = lexicon_fraction(&parsed.terms, EXPLORATORY_LEXICON, denom);

    // Order matches NAV_WEIGHTS/EXPL_WEIGHTS/LOOKUP_WEIGHTS — see the table
    // on their declaration for what each index means and why it is weighted
    // the way it is.
    let features = [
        (term_count.min(MAX_LEN_FEATURE) as f64) / MAX_LEN_FEATURE as f64,
        bool_feature(has_operator),
        bool_feature(has_phrase),
        bool_feature(has_question_word),
        bool_feature(has_known_contact),
        bool_feature(has_date_signal),
        bool_feature(has_entity_pattern),
        lookup_frac,
        exploratory_frac,
    ];

    let navigational = linear_score(NAV_WEIGHTS, NAV_BIAS, &features);
    let exploratory = linear_score(EXPL_WEIGHTS, EXPL_BIAS, &features);
    let lookup = linear_score(LOOKUP_WEIGHTS, LOOKUP_BIAS, &features);

    // Ties resolve Navigational > Exploratory > Lookup: with no feature
    // favoring one class, the tight known-item result set is the safer
    // default over the broad exploratory one, and lookup requires positive
    // entity/lexicon evidence to win at all (see LOOKUP_BIAS).
    if navigational >= exploratory && navigational >= lookup {
        Intent::Navigational
    } else if exploratory >= lookup {
        Intent::Exploratory
    } else {
        Intent::Lookup
    }
}

fn lexicon_fraction(terms: &[parse::Term], lexicon: &[&str], denom: f64) -> f64 {
    let hits = terms
        .iter()
        .filter(|term| lexicon.contains(&term.text.to_lowercase().as_str()))
        .count();
    hits as f64 / denom
}

fn bool_feature(value: bool) -> f64 {
    if value {
        1.0
    } else {
        0.0
    }
}

fn linear_score(weights: [f64; 9], bias: f64, features: &[f64; 9]) -> f64 {
    bias + weights
        .iter()
        .zip(features.iter())
        .map(|(w, f)| w * f)
        .sum::<f64>()
}

/// Free-text term count capped at this many before the length feature
/// saturates to `1.0` — a 20-word query isn't "more exploratory" than a
/// 10-word one in any way this scorer can usefully weigh.
const MAX_LEN_FEATURE: usize = 8;

/// Feature order every weight vector below shares with `classify_intent`'s
/// `features` array — repeated here, next to the numbers themselves, because
/// a weight is meaningless without knowing which feature it multiplies:
///
/// ```text
/// index  feature              intuition encoded below
/// 0      len_norm             short queries lean navigational
/// 1      has_operator         an operator is precise → navigational
/// 2      has_phrase           a quoted phrase is exact → navigational
/// 3      has_question_word    a question is prose → exploratory
/// 4      has_known_contact    "from Acme" is a known item → navigational
/// 5      has_date_signal      a date/recency anchor → navigational
/// 6      has_entity_pattern   a literal ID in the query → lookup
/// 7      lookup_frac          lookup-noun density → lookup
/// 8      exploratory_frac     topic-word density → exploratory
/// ```
///
/// Weights and biases were chosen, not fit — see `classify_intent`'s doc
/// comment — to reproduce prd.md's own worked examples (this module's
/// `intent_classification_matches_the_prds_worked_examples` test); each
/// class's bias encodes how much positive evidence it needs to win at all:
/// navigational is the default outcome for a short, unadorned query
/// (`NAV_BIAS > 0`), lookup requires the entity/lexicon features to argue for
/// it (`LOOKUP_BIAS < 0`), and exploratory sits at the neutral midpoint,
/// winning only once its own features (question words, topic-word density,
/// length) outweigh the other two.
const NAV_WEIGHTS: [f64; 9] = [-0.6, 2.2, 1.4, -2.0, 2.0, 1.6, 0.0, -0.5, -1.0];
const NAV_BIAS: f64 = 0.4;
const EXPL_WEIGHTS: [f64; 9] = [1.4, -1.2, -0.8, 2.4, -0.4, -0.6, -1.0, -0.8, 3.0];
const EXPL_BIAS: f64 = 0.0;
const LOOKUP_WEIGHTS: [f64; 9] = [-0.2, -0.4, -0.4, -1.5, -0.3, -0.2, 2.5, 3.5, -0.8];
const LOOKUP_BIAS: f64 = -0.3;

/// First-word cues that a query is a sentence, not a set of search terms —
/// the same signal prd.md's Stage 0 step 2 lists as a feature
/// ("question-words") and step 7 uses to decide whether Claude should
/// compile a plan.
const QUESTION_WORDS: &[&str] = &[
    "who", "what", "when", "where", "why", "how", "which", "whose", "did", "does", "do", "should",
    "could", "would", "can", "is", "are", "am",
];

/// Free-text words that, in isolation, suggest the user wants a structured
/// fact more than a topic — prd.md's lookup/entity examples ("tracking
/// number for my order", "AWS bill") are built from exactly these.
const LOOKUP_LEXICON: &[&str] = &[
    "tracking",
    "track",
    "bill",
    "confirmation",
    "receipt",
    "order",
    "number",
    "account",
    "reference",
    "statement",
    "package",
    "parcel",
    "shipment",
    "balance",
];

/// Free-text words that suggest a broad topical search rather than a single
/// known item — prd.md's exploratory example ("everything about the office
/// move") is built from two of these.
const EXPLORATORY_LEXICON: &[&str] = &[
    "everything",
    "about",
    "anything",
    "related",
    "regarding",
    "stuff",
    "discussion",
    "discussions",
    "thread",
    "threads",
    "mentions",
    "topics",
    "overview",
];

/// Common short words excluded from contact name/address matching, so "the",
/// "for", "any" can't false-match a contact whose name happens to contain
/// them.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "was", "that", "this", "with", "from", "have", "has", "not", "but",
    "you", "your", "all", "any", "can", "did", "she", "him", "her", "his", "its", "our",
];

/// Relative-date phrases free text can carry ("the invoice ... last week")
/// that the operator grammar's `before:`/`after:`/`on:` never see, because
/// they were never typed as an operator value.
const RELATIVE_DATE_PHRASES: &[&str] = &[
    "today",
    "yesterday",
    "last week",
    "this week",
    "last month",
    "this month",
    "last year",
    "this year",
];

fn starts_with_question_word(raw: &str) -> bool {
    raw.split_whitespace()
        .next()
        .is_some_and(|word| QUESTION_WORDS.contains(&word.to_lowercase().as_str()))
}

fn contains_relative_date_phrase(raw: &str) -> bool {
    let lower = raw.to_lowercase();
    RELATIVE_DATE_PHRASES
        .iter()
        .any(|phrase| lower.contains(phrase))
}

fn is_stopword(text: &str) -> bool {
    STOPWORDS.contains(&text.to_lowercase().as_str())
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// Whether `text` is a single ordinary word worth spell-checking or
/// PMI-pivoting on — long enough to correct/expand safely, and free of
/// characters (`@`, embedded punctuation, ...) that make "the nearest
/// vocabulary word" a meaningless question.
fn is_plain_word(text: &str) -> bool {
    text.chars().count() >= MIN_WORD_LEN && text.chars().all(char::is_alphanumeric)
}

/// Strip combining diacritical marks (NFD-decompose, then drop U+0300..=
/// U+036F) — a close approximation of what `unicode61 remove_diacritics 2`
/// does to every term before it lands in `fts_messages`/`fts_messages_vocab`
/// (V9). "Close" because SQLite's real implementation covers a broader table
/// of legacy precomposed characters `remove_diacritics 2` added over `1`;
/// the combining-mark range alone is what the overwhelming majority of
/// accented Latin text — the case this exists for — actually decomposes to.
/// Used to fold a *query* term before comparing it against vocabulary that
/// was already folded at index time; never needed on the vocabulary side.
fn fold_diacritics(text: &str) -> String {
    text.nfd()
        .filter(|c| !('\u{0300}'..='\u{036f}').contains(c))
        .collect()
}

/// The largest edit distance worth considering for a word of this length.
/// `SymSpell`'s usual default is 2; this scales down for short words for the
/// same reason a 2-letter difference matters more in a 4-letter word than a
/// 10-letter one — below length 4 nothing is offered at all.
fn edit_budget(len: usize) -> usize {
    match len {
        0..=3 => 0,
        4..=5 => 1,
        _ => 2,
    }
}

/// Levenshtein edit distance (insert/delete/substitute, each costing 1).
///
/// No transposition (Damerau's fourth operation): every case spell-fix needs
/// to handle here — an omitted or doubled letter — is already an insertion or
/// deletion, and adding transposition would only broaden what counts as
/// "close enough" without a concrete correction that needs it.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// Split text into lowercased alphanumeric runs — a simple, dependency-free
/// approximation of the `unicode61` tokenizer, good enough for *discovering*
/// PMI candidates (their document frequency is then read from the real
/// tokenizer's own vocabulary table, so a discovery mismatch costs a missed
/// candidate, never a wrong one).
fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

/// Escape `%`, `_` and the escape character itself, so a candidate taken
/// straight from free text can't be read as a `LIKE` wildcard — `%` in "50%
/// off" must match the literal characters, not "anything".
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch == '%' || ch == '_' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests;
