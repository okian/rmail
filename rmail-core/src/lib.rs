//! Core logic shared across the rmail daemon and clients.
//!
//! For the scaffold this crate hosts the runtime path conventions and the
//! Unix-domain-socket gRPC client connector shared by `rmaild` (its tests) and
//! the `mail` CLI. Domain, storage, sync, index, search, and AI subsystems land
//! in later tasks.

pub mod transport;

pub use transport::{connect_uds, default_socket_path, socket_path_from_env, SOCKET_ENV};
