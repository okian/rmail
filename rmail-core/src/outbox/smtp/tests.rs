//! Submission against a real (if in-process) SMTP server.
//!
//! The classification tests deliberately drive `LettreSender` against a server
//! genuinely answering `451` or `550` rather than constructing a
//! `lettre::Error` by hand. The thing being tested is a mapping from *what a
//! server said* to a retry decision, and a hand-built error tests only that the
//! `match` arms are spelled correctly.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::outbox::mock::{MockSmtp, MockSmtpConfig};
use crate::outbox::tests::Fixture;

fn envelope() -> SendEnvelope {
    SendEnvelope {
        from: "alice@example.com".to_owned(),
        to: vec!["bob@example.com".to_owned()],
    }
}

const MESSAGE: &[u8] =
    b"From: alice@example.com\r\nTo: bob@example.com\r\nMessage-ID: <m1@example.com>\r\n\
      Subject: hi\r\n\r\nhello\r\n";

async fn sender_against(fixture: &Fixture, config: MockSmtpConfig) -> (MockSmtp, LettreSender) {
    let mock = MockSmtp::start(config).await.unwrap();
    fixture.set_smtp_port(mock.port());
    let sender = LettreSender::new(fixture.db.clone(), SmtpSecurity::Plaintext);
    (mock, sender)
}

#[tokio::test]
async fn a_successful_submission_delivers_the_octets_verbatim() {
    let fixture = Fixture::open_named("smtp");
    let (mock, sender) = sender_against(&fixture, MockSmtpConfig::default()).await;

    sender
        .send(fixture.account_id, &envelope(), MESSAGE)
        .await
        .unwrap();

    let accepted = mock.accepted();
    assert_eq!(accepted.len(), 1);
    let body = String::from_utf8(accepted[0].clone()).unwrap();
    assert!(body.contains("Message-ID: <m1@example.com>"), "{body}");
    assert!(body.contains("hello"), "{body}");
}

#[tokio::test]
async fn a_4xx_is_transient_and_a_5xx_is_permanent() {
    // The distinction the whole retry policy rests on, taken from a server
    // that actually said the number.
    let fixture = Fixture::open_named("smtp-4xx");
    let (_mock, sender) = sender_against(
        &fixture,
        MockSmtpConfig {
            data_reply: "451 4.3.0 Try again later".to_owned(),
            ..MockSmtpConfig::default()
        },
    )
    .await;
    let failure = sender
        .send(fixture.account_id, &envelope(), MESSAGE)
        .await
        .unwrap_err();
    assert!(failure.is_transient(), "451 should retry: {failure:?}");
    assert!(
        failure.message().contains("451"),
        "the reply code is what tells a user whether to wait or fix: {failure:?}"
    );

    let fixture = Fixture::open_named("smtp-5xx");
    let (_mock, sender) = sender_against(
        &fixture,
        MockSmtpConfig {
            data_reply: "554 5.7.1 Message rejected".to_owned(),
            ..MockSmtpConfig::default()
        },
    )
    .await;
    let failure = sender
        .send(fixture.account_id, &envelope(), MESSAGE)
        .await
        .unwrap_err();
    assert!(!failure.is_transient(), "554 must not retry: {failure:?}");
}

#[tokio::test]
async fn a_rejected_recipient_is_permanent() {
    let fixture = Fixture::open_named("smtp-rcpt");
    let (_mock, sender) = sender_against(
        &fixture,
        MockSmtpConfig {
            rcpt_reply: "550 5.1.1 No such user here".to_owned(),
            ..MockSmtpConfig::default()
        },
    )
    .await;
    let failure = sender
        .send(fixture.account_id, &envelope(), MESSAGE)
        .await
        .unwrap_err();
    assert!(!failure.is_transient(), "{failure:?}");
}

#[tokio::test]
async fn a_server_that_vanishes_mid_send_is_transient() {
    // "The laptop was closed" and "the relay restarted" look identical from
    // here, and neither is a reason to mark a message failed.
    let fixture = Fixture::open_named("smtp-drop");
    let (_mock, sender) = sender_against(
        &fixture,
        MockSmtpConfig {
            drop_after_data: true,
            ..MockSmtpConfig::default()
        },
    )
    .await;
    let failure = sender
        .send(fixture.account_id, &envelope(), MESSAGE)
        .await
        .unwrap_err();
    assert!(failure.is_transient(), "{failure:?}");
}

#[tokio::test]
async fn an_unreachable_server_is_transient_not_a_failed_message() {
    let fixture = Fixture::open_named("smtp-offline");
    // A port nothing is listening on: the offline case, which prd.md says
    // must never fail a message on its own.
    fixture.set_smtp_port(1);
    let sender = LettreSender::new(fixture.db.clone(), SmtpSecurity::Plaintext);
    let failure = sender
        .send(fixture.account_id, &envelope(), MESSAGE)
        .await
        .unwrap_err();
    assert!(failure.is_transient(), "{failure:?}");
}

#[tokio::test]
async fn an_account_with_no_smtp_server_fails_permanently() {
    let fixture = Fixture::open_named("smtp-unconfigured");
    fixture
        .db
        .write({
            let account_id = fixture.account_id;
            move |c| {
                c.execute(
                    "UPDATE accounts SET smtp_server = NULL WHERE id = ?1",
                    [account_id],
                )
            }
        })
        .await
        .unwrap();
    let sender = LettreSender::new(fixture.db.clone(), SmtpSecurity::Auto);
    let failure = sender
        .send(fixture.account_id, &envelope(), MESSAGE)
        .await
        .unwrap_err();
    assert!(
        !failure.is_transient(),
        "retrying an unconfigured account forever helps nobody: {failure:?}"
    );
}

#[tokio::test]
async fn an_envelope_this_build_cannot_form_is_permanent() {
    let fixture = Fixture::open_named("smtp-envelope");
    let (_mock, sender) = sender_against(&fixture, MockSmtpConfig::default()).await;

    for envelope in [
        SendEnvelope {
            from: "not an address".to_owned(),
            to: vec!["bob@example.com".to_owned()],
        },
        SendEnvelope {
            from: "alice@example.com".to_owned(),
            to: vec!["also not one".to_owned()],
        },
        SendEnvelope {
            from: "alice@example.com".to_owned(),
            to: Vec::new(),
        },
    ] {
        let failure = sender
            .send(fixture.account_id, &envelope, MESSAGE)
            .await
            .unwrap_err();
        assert!(
            !failure.is_transient(),
            "the octets are frozen, so every retry reproduces this: {failure:?}"
        );
    }
}

#[tokio::test]
async fn a_transport_is_built_once_per_account_and_reused() {
    // prd.md's "per-account connection reuse". The observable consequence is
    // that a second send does not pay for a second handshake, which here shows
    // up as the same transport instance answering both.
    let fixture = Fixture::open_named("smtp-pool");
    let (mock, sender) = sender_against(&fixture, MockSmtpConfig::default()).await;
    for _ in 0..3 {
        sender
            .send(fixture.account_id, &envelope(), MESSAGE)
            .await
            .unwrap();
    }
    assert_eq!(mock.accepted_count(), 3);
    assert_eq!(sender.lock().len(), 1);
}

#[test]
fn auto_security_never_resolves_to_plaintext() {
    // Silently downgrading a submission on a heuristic is how credentials
    // reach the wire. `Auto` picks between the two encrypted forms and
    // nothing else; plaintext is opt-in by name.
    for port in [25u16, 465, 587, 2525, 1] {
        assert_ne!(SmtpSecurity::Auto.resolve(port), SmtpSecurity::Plaintext);
    }
    assert_eq!(
        SmtpSecurity::Auto.resolve(465),
        SmtpSecurity::ImplicitTls,
        "465 is SMTPS: STARTTLS would wait forever for a banner that is already encrypted"
    );
    assert_eq!(SmtpSecurity::Auto.resolve(587), SmtpSecurity::Starttls);
    // An explicit choice is never overridden by the port.
    assert_eq!(
        SmtpSecurity::Plaintext.resolve(465),
        SmtpSecurity::Plaintext
    );
    assert_eq!(SmtpSecurity::Starttls.resolve(465), SmtpSecurity::Starttls);
}

#[test]
fn a_local_failure_is_permanent_only_when_a_retry_would_reproduce_it() {
    // The lost-mail direction: a busy database or an internal hiccup on the
    // way to the socket must not spend the message's attempt budget as though
    // the server had rejected it.
    for error in [
        Error::not_found("account 7 not found"),
        Error::failed_precondition("account has no SMTP server configured"),
        Error::invalid_argument("malformed address"),
        Error::unauthenticated("keychain item rejected"),
    ] {
        assert!(
            !classify_core_error(&error).is_transient(),
            "{error} should not be retried"
        );
    }
    for error in [
        Error::unavailable("database is busy"),
        Error::deadline_exceeded("credential command timed out"),
        Error::resource_exhausted("connection pool exhausted"),
    ] {
        assert!(
            classify_core_error(&error).is_transient(),
            "{error} should be retried"
        );
    }

    // An internal error is retried, and its detail stays out of the
    // client-readable `last_error`.
    let internal = classify_core_error(&Error::internal("db password = hunter2"));
    assert!(internal.is_transient());
    assert!(
        !internal.message().contains("hunter2"),
        "internal detail must not reach a field a mail.read client can read: {internal:?}"
    );
}

#[test]
fn a_send_failure_maps_onto_the_error_contract() {
    assert_eq!(
        Error::from(SendFailure::Transient("451".to_owned())).reason(),
        crate::ErrorReason::Unavailable
    );
    assert_eq!(
        Error::from(SendFailure::Permanent("550".to_owned())).reason(),
        crate::ErrorReason::FailedPrecondition
    );
}
