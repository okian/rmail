//! Pure-logic unit tests for `mail hook add`'s TOML rendering: the escaper
//! and the event-wire mapping written straight into the operator's config
//! file. End-to-end coverage (a real `mail hook add` round-tripped through
//! `Config::load`, then `list`/`test` against a running daemon) lives in
//! `rmail-cli/tests/hook_cli.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn every_event_arg_wire_form_matches_the_core_config_vocabulary() {
    // Pinned independently of `rmail_core::config::HookEvent`'s own
    // `serde(rename_all = "snake_case")` derivation — if the two ever
    // drift, `mail hook add` would write a value the daemon's config loader
    // rejects, and this is the test that would catch it without needing a
    // round trip through a real file.
    for (arg, expected) in [
        (EventArg::OnNewMessage, "on_new_message"),
        (EventArg::OnLabel, "on_label"),
        (EventArg::OnMove, "on_move"),
        (EventArg::OnRuleMatch, "on_rule_match"),
        (EventArg::OnSyncError, "on_sync_error"),
    ] {
        assert_eq!(arg.wire(), expected);
    }
}

#[test]
fn toml_string_escapes_quotes_and_backslashes() {
    assert_eq!(toml_string("plain"), "\"plain\"");
    assert_eq!(toml_string("a\"b"), "\"a\\\"b\"");
    assert_eq!(toml_string("a\\b"), "\"a\\\\b\"");
}

#[test]
fn toml_string_escapes_control_characters() {
    assert_eq!(toml_string("a\nb"), "\"a\\nb\"");
    assert_eq!(toml_string("a\tb"), "\"a\\tb\"");
    assert_eq!(toml_string("a\rb"), "\"a\\rb\"");
    // U+007F (DEL) is outside TOML's `basic-unescaped` range
    // (`%x20-21 / %x23-5B / %x5D-7E / non-ascii`) just like the other C0
    // controls above, even though it is not conventionally called a
    // "control character" in casual usage.
    assert_eq!(toml_string("a\u{7f}b"), "\"a\\u007fb\"");
}

#[test]
fn toml_string_round_trips_arbitrary_values_through_a_real_config_parse() {
    // The contract that actually matters: whatever this escapes must be
    // exactly what `Config::from_toml_str` — the same validation `add`
    // itself runs before writing anything — reads back, for the kind of
    // input an operator's own `--name`/command/args could plausibly carry.
    for raw in [
        "simple",
        "has spaces",
        "has \"quotes\"",
        "has\\backslashes",
        "has\ttabs\nand\nnewlines",
        "emoji \u{1F389} and \u{dc}n\u{ef}c\u{f6}d\u{e9}",
    ] {
        let toml = format!(
            "[[hooks.hooks]]\nname = {}\nevent = \"on_new_message\"\ncommand = \"/bin/true\"\n",
            toml_string(raw)
        );
        let cfg = rmail_core::Config::from_toml_str(&toml)
            .unwrap_or_else(|e| panic!("escaped value must be valid TOML/config: {e}\n{toml}"));
        assert_eq!(cfg.hooks.hooks[0].name, raw);
    }
}
