//! Generated protobuf/gRPC types for the rmail service.
//!
//! The module tree mirrors the proto package hierarchy: [`v1`] contains the
//! `rmail.v1` messages and (in later tasks) service stubs. Generated code is
//! exempted from the workspace lint denials since it is not hand-written.

/// Types generated from the `rmail.v1` proto package.
pub mod v1 {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::todo)]
    // `AiService`'s `AnalyzeEvent` oneof (task 50) mixes a tiny `Token(String)`
    // variant with a `Done(Summary)` variant carrying a whole merged summary —
    // an intentional shape (a stream's terminal frame legitimately carries more
    // than its per-token frames), not something `build.rs` (off limits — see
    // that file's own header) can be told to box per-field via prost-build's
    // `.boxed()` option without touching it.
    #![allow(clippy::large_enum_variant)]
    tonic::include_proto!("rmail.v1");
}

/// Encoded protobuf `FileDescriptorSet` for every compiled proto in this crate.
///
/// Registered with `tonic-reflection` so gRPC reflection can describe the API.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/rmail_descriptor.bin"));
