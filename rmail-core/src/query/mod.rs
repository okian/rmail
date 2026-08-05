//! Query understanding: turning what a user typed into a structured request
//! the retrieval and ranking pipeline can act on (prd.md, "Stage 0 — Query
//! Understanding").
//!
//! [`parse`] is the ground floor of that pipeline — deterministic operator
//! parsing with no ranking, no natural-language handling, and no I/O. It
//! turns `from:alice -tag:newsletter "office move"` into the operators that
//! must gate the result set and the free text that should rank it, and
//! nothing more. Intent classification, spelling correction, alias
//! resolution, query expansion, and embedding — the rest of Stage 0 — build
//! on top of [`ParsedQuery`] in a later task; this module does not attempt
//! any of them.

pub mod parse;

pub use parse::{
    parse, AiPredicate, Filter, HasTarget, IsFlag, Mode, Operator, ParsedQuery, Phrase, Term,
};
