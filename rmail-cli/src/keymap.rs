//! The keymap engine, re-exported from `rmail-core`.
//!
//! The types live in the core crate rather than here because `rmaild` needs
//! them too: task 84's acceptance calls for action ids "shared by
//! palette/gRPC/MCP", and `ConfigService.GetKeymap/SetBinding` cannot validate
//! a binding against a registry it has no way to see. Keeping one copy is also
//! what stops the daemon and the CLI from growing two `keys.toml` parsers that
//! disagree about the same file.
//!
//! This module stays as a re-export so the TUI's own paths (`crate::keymap::…`)
//! read the way the rest of the crate does.
pub use rmail_core::keymap::*;
