//! Analytics over the local mailbox mirror: questions about the *shape* of a
//! correspondence rather than about any one message.
//!
//! Everything here is derived, read-only and model-free — headers and folder
//! names in, numbers out. Nothing in this module writes a row, reaches IMAP,
//! or sends text to a provider, which is why the RPCs it backs are `Read` in
//! `crate::parity` and sit behind `mail.read`.
//!
//! [`response_time`] is prd.md feature 58 (task 71).

pub mod response_time;

pub use response_time::{
    response_times, GroupBy, ResponseGroup, ResponseTimeQuery, ResponseTimes, Stats, TrendPoint,
};
