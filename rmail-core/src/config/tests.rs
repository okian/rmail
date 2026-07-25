//! Unit tests for the configuration system: defaults, env overlay, file
//! loading, and the unknown-key / bad-value / missing-file error paths.
//
// Every test that parses config runs inside a `figment::Jail`. `Jail` mutates
// the process-global environment and serializes Jail-vs-Jail via an internal
// lock; `clear_env()` strips ambient `RMAIL_*` vars. Doing this in every parsing
// test keeps the suite hermetic under `cargo test` (threaded), not just under
// nextest's process-per-test isolation.
//
// `figment::Jail::expect_with` dictates a `Result<(), figment::Error>` closure
// signature; `figment::Error` is large, so this lint is unavoidable here.
#![allow(clippy::result_large_err)]

use std::time::Duration;

use figment::Jail;

use super::*;

/// Convert any displayable error into a `figment::Error` for `Jail` closures.
fn fe<E: std::fmt::Display>(err: E) -> figment::Error {
    figment::Error::from(err.to_string())
}

#[test]
fn defaults_match_prd() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::from_toml_str("").map_err(fe)?;

        // sync
        assert_eq!(cfg.sync.interval.as_duration(), Duration::from_secs(300));
        assert!(cfg.sync.idle);
        assert!(cfg.sync.qresync);

        // search
        assert_eq!(cfg.search.default_mode, SearchMode::Hybrid);
        assert_eq!(cfg.search.fusion, Fusion::Rrf);
        assert_eq!(cfg.search.rrf_k, 60);
        assert_eq!(cfg.search.rerank, Rerank::Auto);
        assert!((cfg.search.mmr_lambda - 0.7).abs() < f64::EPSILON);
        assert!((cfg.search.bm25_weights.subject - 8.0).abs() < f64::EPSILON);
        assert!((cfg.search.fusion_weights.navigational.lexical - 1.0).abs() < f64::EPSILON);
        assert!((cfg.search.fusion_weights.exploratory.dense - 1.0).abs() < f64::EPSILON);
        assert!((cfg.search.rank_weights.is_newsletter + 0.40).abs() < f64::EPSILON);

        // index — privacy default is local embeddings
        assert_eq!(cfg.index.workers, 4);
        assert_eq!(cfg.index.semantic.provider, SemanticProvider::Local);
        assert_eq!(cfg.index.semantic.local.dim, 384);
        assert_eq!(cfg.index.extract.max_attachment_mb, 25);

        // ai
        assert!(cfg.ai.enabled);
        assert_eq!(cfg.ai.provider, AiProvider::Claude);
        assert_eq!(cfg.ai.models.triage, "claude-haiku-4-5");
        assert_eq!(cfg.ai.models.deep, "claude-opus-4-8");
        assert_eq!(cfg.ai.models.embedding, EmbeddingBackend::Local);
        assert!((cfg.ai.limits.daily_cost_cap_usd - 5.00).abs() < f64::EPSILON);
        assert_eq!(cfg.ai.limits.on_cap, OnCap::Pause);
        assert_eq!(
            cfg.ai.prompt_cache.ttl.as_duration(),
            Duration::from_secs(3_600)
        );

        // tags / notes
        assert_eq!(cfg.tags.default_sync_mode, TagSyncMode::Auto);
        assert_eq!(cfg.tags.imap.keyword_prefix, "rmail/");
        assert!((cfg.tags.ai.auto_apply_min_confidence - 0.85).abs() < f64::EPSILON);
        assert_eq!(cfg.tags.ai.taxonomy.len(), 8);
        assert_eq!(cfg.notes.preview_lines, 6);

        // send
        assert_eq!(cfg.send.undo_window.as_duration(), Duration::from_secs(10));
        assert_eq!(
            cfg.send.followup.default_delay.as_duration(),
            Duration::from_secs(259_200)
        );

        // finder
        assert_eq!(cfg.finder.default_scope, FinderScope::All);
        assert_eq!(cfg.finder.refresh_interval_ms, 250);

        // grpc
        assert!(cfg.grpc.enabled);
        assert_eq!(cfg.grpc.auth, GrpcAuth::Token);
        assert_eq!(cfg.grpc.limits.max_message_bytes, 16_777_216);
        assert_eq!(cfg.grpc.events.retention_days, 7);

        // no accounts by default
        assert!(cfg.accounts.is_empty());
        Ok(())
    });
}

#[test]
fn full_config_parses_and_resolves() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let toml = r#"
[[accounts]]
name = "Personal"
imap_server = "imap.fastmail.com"
port = 993
username = "user@example.com"
password_command = "security find-generic-password -s fastmail -w"
smtp_server = "smtp.fastmail.com"
smtp_port = 587

[[accounts]]
name = "Personal-Legal"
[accounts.ai]
enabled = false
residency = "us"

[sync]
interval = "5m"
idle = true
qresync = true

[search]
default_mode = "hybrid"
rerank = "auto"
learning = true

[index]
enabled = true
workers = 8

[index.semantic]
provider = "voyage"

[ai]
enabled = true
provider = "claude"

[ai.models]
triage = "claude-haiku-4-5"
deep = "claude-sonnet-5"

[ai.limits]
daily_cost_cap_usd = 12.5

[grpc]
auth = "token"
"#;
        let cfg = Config::from_toml_str(toml).map_err(fe)?;

        assert_eq!(cfg.accounts.len(), 2);
        assert_eq!(cfg.accounts[0].name, "Personal");
        assert_eq!(cfg.accounts[0].port, 993);
        assert_eq!(cfg.accounts[0].smtp_port, 587);
        assert!(
            cfg.accounts[0].ai.enabled,
            "first account inherits AI-enabled default"
        );

        // second account: minimal, hard AI opt-out
        assert_eq!(cfg.accounts[1].port, 993, "port filled from default");
        assert!(!cfg.accounts[1].ai.enabled);
        assert_eq!(cfg.accounts[1].ai.residency.as_deref(), Some("us"));

        // overrides applied, rest defaulted
        assert_eq!(cfg.index.workers, 8);
        assert_eq!(cfg.index.semantic.provider, SemanticProvider::Voyage);
        assert_eq!(cfg.ai.models.deep, "claude-sonnet-5");
        assert!((cfg.ai.limits.daily_cost_cap_usd - 12.5).abs() < f64::EPSILON);
        assert_eq!(cfg.ai.models.triage, "claude-haiku-4-5");
        Ok(())
    });
}

#[test]
fn env_overrides_file_and_defaults() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        jail.set_env("RMAIL_AI__ENABLED", "false");
        jail.set_env("RMAIL_GRPC__AUTH", "none");
        jail.set_env("RMAIL_SEARCH__RRF_K", "99");
        jail.set_env("RMAIL_SYNC__INTERVAL", "2h");

        let cfg = Config::from_toml_str("[ai]\nenabled = true\n").map_err(fe)?;

        assert!(!cfg.ai.enabled, "env overrides the file value");
        assert_eq!(cfg.grpc.auth, GrpcAuth::None);
        assert_eq!(cfg.search.rrf_k, 99);
        assert_eq!(cfg.sync.interval.as_duration(), Duration::from_secs(7_200));
        Ok(())
    });
}

#[test]
fn env_ignores_unrelated_rmail_vars() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        // Neither the daemon's bootstrap socket var, an unknown table, nor a
        // single-underscore `RMAIL_<table>_x` var must break the load.
        jail.set_env("RMAIL_SOCKET", "/tmp/whatever.sock");
        jail.set_env("RMAIL_LOG", "debug");
        jail.set_env("RMAIL_SYNC_TOKEN", "should-be-ignored");

        let cfg = Config::from_toml_str("").map_err(fe)?;
        assert!(cfg.ai.enabled);
        assert!(cfg.sync.idle);
        assert!(cfg.accounts.is_empty());
        Ok(())
    });
}

#[test]
fn every_known_table_accepts_an_override() {
    // Guards against a typo in KNOWN_TABLES silently dropping a table's
    // overrides: each scalar override must actually take effect. (`accounts` is
    // an array and is exercised via file parsing instead.)
    Jail::expect_with(|jail| {
        jail.clear_env();
        jail.set_env("RMAIL_SYNC__IDLE", "false");
        jail.set_env("RMAIL_SEARCH__RRF_K", "61");
        jail.set_env("RMAIL_INDEX__WORKERS", "9");
        jail.set_env("RMAIL_AI__ENABLED", "false");
        jail.set_env("RMAIL_TAGS__HIERARCHY_SEPARATOR", ":");
        jail.set_env("RMAIL_NOTES__INDEX", "false");
        jail.set_env("RMAIL_SEND__MAX_RETRIES", "9");
        jail.set_env("RMAIL_FINDER__ENABLED", "false");
        jail.set_env("RMAIL_GRPC__ENABLED", "false");

        let cfg = Config::from_toml_str("").map_err(fe)?;
        assert!(!cfg.sync.idle);
        assert_eq!(cfg.search.rrf_k, 61);
        assert_eq!(cfg.index.workers, 9);
        assert!(!cfg.ai.enabled);
        assert_eq!(cfg.tags.hierarchy_separator, ":");
        assert!(!cfg.notes.index);
        assert_eq!(cfg.send.max_retries, 9);
        assert!(!cfg.finder.enabled);
        assert!(!cfg.grpc.enabled);
        Ok(())
    });
}

#[test]
fn loads_from_file_with_partial_tables() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        jail.create_file("rmail.toml", "[ai]\nenabled = false\n")?;
        // Jail sets cwd to its temp dir, so the relative path resolves there.
        let cfg = Config::load("rmail.toml").map_err(fe)?;

        assert!(!cfg.ai.enabled);
        // A present-but-partial [ai] table still fills every other field.
        assert_eq!(cfg.ai.models.triage, "claude-haiku-4-5");
        assert_eq!(cfg.ai.limits.on_cap, OnCap::Pause);
        Ok(())
    });
}

#[test]
fn partial_nested_intent_table_fills_neutral() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        // Only one source specified in a nested intent weight table; the rest
        // must fill from the neutral 1.0 default rather than error.
        let cfg = Config::from_toml_str("[search.fusion_weights.navigational]\nlexical = 2.0\n")
            .map_err(fe)?;
        assert!((cfg.search.fusion_weights.navigational.lexical - 2.0).abs() < f64::EPSILON);
        assert!((cfg.search.fusion_weights.navigational.dense - 1.0).abs() < f64::EPSILON);
        assert!((cfg.search.fusion_weights.navigational.recency - 1.0).abs() < f64::EPSILON);
        Ok(())
    });
}

#[test]
fn missing_file_is_not_found() {
    // Reaches NotFound before any figment/env work, so no Jail is required.
    let err = Config::load("/nonexistent/rmail/definitely-absent.toml")
        .expect_err("missing file should error");
    assert!(
        matches!(err, ConfigError::NotFound(_)),
        "expected NotFound, got: {err}"
    );
}

#[test]
fn load_or_default_tolerates_missing_file() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let cfg = Config::load_or_default("does-not-exist.toml").map_err(fe)?;
        assert!(cfg.ai.enabled);
        assert_eq!(cfg.grpc.auth, GrpcAuth::Token);
        Ok(())
    });
}

#[test]
fn unknown_key_is_rejected() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let err = Config::from_toml_str("[ai]\nenabled = true\nbogus_field = 1\n")
            .expect_err("unknown key should error");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("bogus_field") || msg.contains("unknown"),
            "message should name the offending key: {err}"
        );
        Ok(())
    });
}

#[test]
fn bad_enum_value_is_rejected() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let err = Config::from_toml_str("[grpc]\nauth = \"bogus\"\n")
            .expect_err("bad enum value should error");
        assert!(
            matches!(err, ConfigError::Invalid(_)),
            "expected Invalid, got: {err}"
        );
        assert!(
            err.to_string().contains("bogus"),
            "message should name the bad value: {err}"
        );
        Ok(())
    });
}

#[test]
fn bad_duration_is_rejected() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        let err = Config::from_toml_str("[sync]\ninterval = \"5x\"\n")
            .expect_err("bad duration should error");
        assert!(
            matches!(err, ConfigError::Invalid(_)),
            "expected Invalid, got: {err}"
        );
        assert!(
            err.to_string().contains("5x"),
            "message should name the bad duration: {err}"
        );
        Ok(())
    });
}

#[test]
fn bad_env_value_is_rejected() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        // A malformed *env* override must error, not be silently ignored.
        jail.set_env("RMAIL_SYNC__INTERVAL", "not-a-duration");
        let err = Config::from_toml_str("").expect_err("bad env value should error");
        assert!(
            matches!(err, ConfigError::Invalid(_)),
            "expected Invalid, got: {err}"
        );
        Ok(())
    });
}

#[test]
fn inline_password_is_rejected() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        // Enforces the no-inline-secrets rule: `password` is not a known field.
        let toml = "[[accounts]]\nname = \"x\"\npassword = \"hunter2\"\n";
        let err = Config::from_toml_str(toml).expect_err("inline password should error");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("password") || msg.contains("unknown"),
            "message: {err}"
        );
        Ok(())
    });
}

#[test]
fn socket_path_tilde_expansion() {
    Jail::expect_with(|jail| {
        jail.clear_env();
        jail.set_env("HOME", "/home/rmail-test");
        let cfg = Config::from_toml_str("").map_err(fe)?;
        assert_eq!(
            cfg.grpc.resolved_socket_path(),
            std::path::PathBuf::from("/home/rmail-test/.local/state/rmail/rmaild.sock")
        );
        Ok(())
    });
}
