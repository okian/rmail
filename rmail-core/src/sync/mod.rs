//! Mailbox synchronization.
//!
//! [`full`] is the initial sync: a UID-window walk that downloads a folder
//! newest-first and is resumable by construction. Delta sync (CONDSTORE /
//! QRESYNC) and the IDLE push engine land alongside it in later tasks.

pub mod full;

pub use full::{
    prioritize, sync_folder, sync_folders, SyncOptions, SyncProgress, SyncReport, DEFAULT_WINDOW,
};
