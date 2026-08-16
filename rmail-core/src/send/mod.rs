//! Send-time gates: the checks that stand between a finished message and the
//! outbox.
//!
//! [`crate::outbox`] owns *how* a message is transmitted — the durable row,
//! the at-most-once fence, the lease, the retry ladder. This module owns the
//! separate question of *whether it should be*, and the two are deliberately
//! not the same crate module: everything in `outbox` is about not losing and
//! not duplicating a message the user has already committed to, while
//! everything here runs strictly before that commitment and is allowed to say
//! no.
//!
//! The one member today is [`preflight`], the pre-send guardian (prd.md #20,
//! task 63). Its module docs carry the reasoning that matters most in this
//! subtree: what happens when the check cannot be made.

pub mod preflight;
