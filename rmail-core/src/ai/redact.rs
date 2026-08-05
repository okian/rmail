//! The PII redaction firewall that every outbound model call passes through.
//!
//! Implemented by task 44. Declared ahead of it so that the redaction, audit
//! and policy stages — which are built concurrently and each own one file
//! here — do not all have to edit `ai/mod.rs` to announce themselves.
