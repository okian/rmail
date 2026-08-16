//! Core logic shared across the rmail daemon and clients.
//!
//! For the scaffold this crate hosts the runtime path conventions and the
//! Unix-domain-socket gRPC client connector shared by `rmaild` (its tests) and
//! the `mail` CLI. Domain, storage, sync, index, search, and AI subsystems land
//! in later tasks.

pub mod account;
pub mod ai;
pub mod analytics;
pub mod attach;
pub mod auth;
pub mod autoconfig;
pub mod compose;
pub mod config;
pub mod credential;
pub mod digest;
pub mod embed;
pub mod error;
pub mod eval;
pub mod events;
pub mod export;
pub mod features;
pub mod feedback;
pub mod finder;
pub mod fuse;
pub mod hooks;
pub mod idempotency;
pub mod imap;
pub mod index;
pub mod keymap;
pub mod mail;
pub mod message;
pub mod notes;
pub mod notify;
pub mod oauth;
pub mod outbox;
pub mod page;
pub mod parity;
pub mod present;
pub mod query;
pub mod rank;
pub mod repo;
pub mod retrieve;
pub mod rules;
pub mod saved_search;
pub mod send;
pub mod smart_folder;
pub mod storage;
pub mod sync;
pub mod tags;
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
