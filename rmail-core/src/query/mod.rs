//! Query understanding: turning what a user typed into a structured request
//! the retrieval and ranking pipeline can act on (prd.md, "Stage 0 — Query
//! Understanding").
//!
//! [`parse`] is the ground floor of that pipeline — deterministic operator
//! parsing with no ranking, no natural-language handling, and no I/O. It
//! turns `from:alice -tag:newsletter "office move"` into the operators that
//! must gate the result set and the free text that should rank it, and
//! nothing more.
//!
//! [`plan`] builds on top of [`ParsedQuery`]: intent classification, spelling
//! correction against the corpus vocabulary, contact/alias resolution, PMI
//! synonym expansion, date resolution, and the query embedding — the rest of
//! Stage 0 — to produce the [`plan::QueryPlan`] every retriever downstream
//! consumes.

pub mod parse;
pub mod plan;

pub use parse::{
    parse, AiPredicate, Filter, HasTarget, IsFlag, Mode, Operator, ParsedQuery, Phrase, Term,
};
pub use plan::{
    DateRange, EntityRef, EntityRefKind, HardFilter, Intent, PlanTerm, QueryPlan, QueryPlanner,
    Scope, SortSpec, TermOrigin,
};
