//! Core logic shared across the rmail daemon and clients.
//!
//! For the scaffold this crate hosts the runtime path conventions and the
//! Unix-domain-socket gRPC client connector shared by `rmaild` (its tests) and
//! the `mail` CLI. Domain, storage, sync, index, search, and AI subsystems land
//! in later tasks.

pub mod config;
pub mod error;
pub mod repo;
pub mod storage;
pub mod telemetry;
pub mod transport;

pub use config::{Config, ConfigError};
pub use error::{Error, ErrorReason, Result, ERROR_DOMAIN};
pub use storage::{Database, StorageError};
pub use telemetry::{LogFormat, TelemetryError};
pub use transport::{connect_uds, default_socket_path, socket_path_from_env, SOCKET_ENV};
