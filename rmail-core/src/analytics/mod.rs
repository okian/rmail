//! Analytics over the local mailbox mirror: questions about the *shape* of a
//! correspondence rather than about any one message.
//!
//! Nothing here writes a row and nothing here reaches IMAP. What varies is
//! whether a model is involved, and that is the line the scope table is drawn
//! on:
//!
//! | entry point | reads | model | scope |
//! |---|---|---|---|
//! | [`response_times`] | headers | no | `mail.read` |
//! | [`contacts::metrics`] | headers, subjects | no | — |
//! | [`subscriptions::detect`] | headers, `raw` header blocks | no | — |
//! | [`contacts::ContactBriefer`] | the above | **yes** | `mail.read` + `ai.invoke` |
//! | [`subscriptions::SubscriptionClassifier`] | the above | **yes** | `mail.read` + `ai.invoke` |
//! | [`nl::AnalyticsAsker`] | the `analytics_*` views | **yes** | `mail.read` + `ai.invoke` |
//!
//! The split is per entry point, not per module — `contacts::metrics` and
//! `subscriptions::detect` are reachable on a daemon with no provider at all
//! and cost nothing. The *RPCs*, though, are gated as a whole: an RPC whose
//! spend depends on one request field is gated for the case where that field
//! is set, because the scope table gates a method and a caller who can call
//! the method can set the field.
//!
//! [`response_time`] is prd.md feature 58 (task 71). [`contacts`],
//! [`subscriptions`] and [`nl`] are prd.md features 59, 60 and 61 (task 72).
//!
//! # Nothing here acts on the user's behalf
//!
//! [`subscriptions`] detects `List-Unsubscribe` and reports what it found.
//! It never fetches a URL, never follows a redirect and never sends mail:
//! a detected header is a *proposal* for a human to act on, and the header is
//! attacker-authored text besides. See that module's own docs.

pub mod contacts;
pub mod nl;
pub mod response_time;
pub mod sql;
pub mod subscriptions;

pub use contacts::{ContactBriefer, ContactInsight, ContactInsightQuery};
pub use nl::{AnalyticsAnswer, AnalyticsAsker, AnalyticsQuestion};
pub use response_time::{
    response_times, GroupBy, ResponseGroup, ResponseTimeQuery, ResponseTimes, Stats, TrendPoint,
};
pub use subscriptions::{
    Subscription, SubscriptionClassifier, SubscriptionQuery, SubscriptionReport,
};
