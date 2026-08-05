//! Core logic shared across the rmail daemon and clients.
//!
//! For the scaffold this crate hosts the runtime path conventions and the
//! Unix-domain-socket gRPC client connector shared by `rmaild` (its tests) and
//! the `mail` CLI. Domain, storage, sync, index, search, and AI subsystems land
//! in later tasks.

pub mod account;
pub mod config;
pub mod credential;
pub mod embed;
pub mod error;
pub mod events;
pub mod imap;
pub mod index;
pub mod message;
pub mod repo;
pub mod storage;
pub mod sync;
pub mod telemetry;
pub mod thread;
pub mod transport;

pub use config::{Config, ConfigError};
pub use credential::{CredentialSource, Secret};
pub use error::{Error, ErrorReason, Result, ERROR_DOMAIN};
pub use storage::{Database, StorageError};
pub use telemetry::{LogFormat, TelemetryError};
pub use transport::{
    config_path_from_env, connect_uds, db_path_from_env, default_config_path, default_db_path,
    default_socket_path, socket_path_from_env, DB_ENV, SOCKET_ENV,
};
