//! Integration test for the global telemetry init.
//!
//! Runs in its own process (so the global subscriber it installs is isolated
//! from other tests), which lets us exercise the real `init()` entry point and
//! its `AlreadyInitialized` error path without cross-test contamination.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use rmail_core::telemetry::{self, LogFormat, TelemetryError};

#[test]
fn init_installs_once_then_reports_already_initialized() {
    // First init succeeds and installs the global subscriber (text arm).
    telemetry::init(LogFormat::Text).expect("first telemetry init should succeed");

    // A second init constructs the JSON arm's layer, then fails to set the
    // already-installed global — surfaced as a typed, source-carrying error.
    let second = telemetry::init(LogFormat::Json);
    assert!(
        matches!(second, Err(TelemetryError::AlreadyInitialized(_))),
        "second init should report AlreadyInitialized, got: {second:?}"
    );
}
