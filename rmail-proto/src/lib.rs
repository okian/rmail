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
    // Every generated server/client trait method returns `Result<Response<T>,
    // tonic::Status>`, and `tonic::Status` itself is the large side (>= 176
    // bytes; it carries a message, metadata, and an optional source error) —
    // shrinking it is tonic-build's call, not something reachable from this
    // crate's `build.rs` (off limits — see that file's own header), and
    // boxing it would mean every caller in `rmaild`/`rmail-cli` matches on
    // `Box<tonic::Status>` instead of the type tonic's own docs use
    // everywhere. `result_large_err` is a recent addition to clippy's
    // default warn set (first observed on 1.98.0); it did not fire against
    // this same generated output on the toolchain this workspace was last
    // pinned to.
    #![allow(clippy::result_large_err)]
    tonic::include_proto!("rmail.v1");
}

/// Encoded protobuf `FileDescriptorSet` for every compiled proto in this crate.
///
/// Registered with `tonic-reflection` so gRPC reflection can describe the API.
pub const FILE_DESCRIPTOR_SET: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/rmail_descriptor.bin"));
