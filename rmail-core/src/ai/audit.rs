//! The append-only ledger recording every model call, and its cost rollups.
//!
//! Implemented by task 45. Declared ahead of it so that the redaction, audit
//! and policy stages — which are built concurrently and each own one file
//! here — do not all have to edit `ai/mod.rs` to announce themselves.
