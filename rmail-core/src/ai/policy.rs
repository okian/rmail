//! The per-account/folder AI policy and data-residency engine.
//!
//! Implemented by task 46. Declared ahead of it so that the redaction, audit
//! and policy stages — which are built concurrently and each own one file
//! here — do not all have to edit `ai/mod.rs` to announce themselves.
