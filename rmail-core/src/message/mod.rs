//! Message fetching, parsing, and persistence.
//!
//! [`parse`] turns raw RFC822 into storable metadata; [`fetch`] pulls messages
//! over IMAP and persists them idempotently (keyed by mailbox + uidvalidity +
//! uid), so a re-fetch is a no-op.

pub mod fetch;
pub mod parse;

pub use fetch::{fetch_and_persist, fetch_uids, persist_fetched, FetchedMessage, PersistOutcome};
pub use parse::{parse_message, ParsedAttachment, ParsedMessage};
