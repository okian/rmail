//! Integration test: drive `WebhookService` end-to-end against an in-process
//! tonic server over a Unix domain socket.
//!
//! What this file exists to prove, over and above `rmail_core::webhooks`'s own
//! unit tests:
//!
//! - the service is actually *wired into the daemon* — registered on the
//!   router, reachable, and reachable even on a daemon with
//!   `webhooks.enabled = false`, which is the default and the state an
//!   operator is in when they first go to add a destination. Task 57 shipped a
//!   mechanism nobody could enable; this is the test that would have caught
//!   it;
//! - the operator loop closes over the wire: register → list → forward →
//!   inspect the queue → replay → remove;
//! - the URL policy and the duplicate-name rule map onto the right
//!   `tonic::Code`s rather than surfacing as `INTERNAL`;
//! - `ListDeliveries` does not hand back mail content unless the caller asked
//!   for the payload;
//! - a caller cannot name a URL: there is no request field that would let one,
//!   and `Forward` only accepts a registered destination.
//!
//! The scope each of these RPCs is gated on is asserted where the table lives
//! (`rmaild::auth::methods`'
//! `every_webhook_rpc_needs_both_mail_read_and_automation`), next to the other
//! agreement checks, rather than duplicated here.
//!
//! Nothing here reaches the network. `Forward` *queues* a delivery — the
//! dispatcher is off on this daemon — so no socket outside the process is ever
//! opened; the delivery path itself is covered against a loopback server in
//! `rmail_core::webhooks::tests`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::{repo, Config, Database};
use rmail_proto::v1::webhook_service_client::WebhookServiceClient;
use rmail_proto::v1::{
    ForwardMessageRequest, ListDeliveriesRequest, ListWebhooksRequest, RegisterWebhookRequest,
    RemoveWebhookRequest, ReplayDeliveryRequest, SetWebhookEnabledRequest, WebhookDeliveryState,
    WebhookEvent, WebhookSecretSource, WebhookTemplate,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_path(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rmail-webhooksvc-{label}-{pid}-{n}"))
}

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    /// A daemon with `[webhooks]` at its **defaults** — that is,
    /// `enabled = false`. Deliberately: the service must be usable before an
    /// operator has ever switched the dispatcher on, or there is no way to
    /// configure the thing they are about to switch on.
    async fn start() -> Self {
        let socket = unique_path("sock");
        let db_path = unique_path("db");
        let db = Database::open(&db_path).expect("open db");

        let mut config = Config::default();
        config.index.semantic.enabled = false;
        config.ai.enabled = false;
        assert!(
            !config.webhooks.enabled,
            "this suite's whole premise is that the default is off"
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds_with_config(&server_socket, server_db, config, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut ready = false;
        for _ in 0..200 {
            if rmail_core::connect_uds(&socket).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "server never became ready");

        Self {
            socket,
            db_path,
            db,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> WebhookServiceClient<Channel> {
        WebhookServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    /// One message to forward, with a stored triage summary so the enrichment
    /// path is exercised over the wire too.
    async fn message(&self, subject: &str) -> i64 {
        let (account_id, mailbox_id) = self
            .db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .unwrap();
        let subject = subject.to_owned();
        let message_id = self
            .db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid: 1,
                        uidvalidity: 1,
                        subject: Some(subject),
                        from_addr: Some("ada@example.com".to_owned()),
                        from_name: Some("Ada Lovelace".to_owned()),
                        body_text: Some("the confidential body".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO ai_summaries
                       (message_id, account_id, model, pass, schema_version, tl_dr, todos)
                     VALUES (?1, ?2, 'claude-haiku-4-5', 'triage', 1,
                             'Legal wants redlines back. They sign Monday.',
                             '[\"return redlines\"]')",
                    rusqlite::params![message_id, account_id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        message_id
    }

    async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn register(name: &str, url: &str) -> RegisterWebhookRequest {
    RegisterWebhookRequest {
        name: name.to_owned(),
        url: url.to_owned(),
        template: WebhookTemplate::Generic as i32,
        events: vec![WebhookEvent::OnNewMessage as i32],
        include_body: false,
        disabled: false,
        secret_source: WebhookSecretSource::Unspecified as i32,
        secret_reference: String::new(),
        max_attempts: 0,
    }
}

#[tokio::test]
async fn the_operator_loop_closes_over_the_wire() {
    let server = TestServer::start().await;
    let message_id = server.message("Contract review").await;
    let mut client = server.client().await;

    // Register.
    let registered = client
        .register(RegisterWebhookRequest {
            template: WebhookTemplate::Slack as i32,
            secret_source: WebhookSecretSource::Env as i32,
            secret_reference: "RMAIL_WEBHOOK_KEY".to_owned(),
            max_attempts: 3,
            ..register("eng-alerts", "https://hooks.example.com/services/abc")
        })
        .await
        .unwrap()
        .into_inner()
        .destination
        .unwrap();
    assert_eq!(registered.name, "eng-alerts");
    assert_eq!(registered.template, WebhookTemplate::Slack as i32);
    assert_eq!(registered.max_attempts, 3);
    assert!(!registered.include_body, "off unless asked for");
    assert_eq!(registered.secret_source, WebhookSecretSource::Env as i32);
    assert_eq!(
        registered.secret_reference, "RMAIL_WEBHOOK_KEY",
        "the reference is reported; the key itself is never stored to report"
    );

    // List.
    let listed = client
        .list(ListWebhooksRequest { reveal_url: false })
        .await
        .unwrap()
        .into_inner()
        .destinations;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, registered.id);

    // Forward — queued, not sent: the dispatcher is off on this daemon.
    let delivery = client
        .forward(ForwardMessageRequest {
            message_id,
            destination: "eng-alerts".to_owned(),
        })
        .await
        .unwrap()
        .into_inner()
        .delivery
        .unwrap();
    assert_eq!(delivery.state, WebhookDeliveryState::Pending as i32);
    assert_eq!(delivery.event, "forward");
    assert_eq!(delivery.destination_name, "eng-alerts");
    assert_eq!(delivery.message_id, message_id);
    assert_eq!(delivery.attempts, 0);
    assert_eq!(delivery.max_attempts, 3);

    // The queue view, without the payload by default.
    let queued = client
        .list_deliveries(ListDeliveriesRequest {
            destination: "eng-alerts".to_owned(),
            limit: 10,
            include_payload: false,
        })
        .await
        .unwrap()
        .into_inner()
        .deliveries;
    assert_eq!(queued.len(), 1);
    assert!(
        queued[0].payload.is_empty(),
        "a queue listing must not restate mail content unless asked"
    );

    // ...and with it, on request — which is where the enrichment shows up.
    let with_payload = client
        .list_deliveries(ListDeliveriesRequest {
            destination: String::new(),
            limit: 10,
            include_payload: true,
        })
        .await
        .unwrap()
        .into_inner()
        .deliveries;
    let payload: serde_json::Value = serde_json::from_str(&with_payload[0].payload).unwrap();
    assert_eq!(payload["message"]["subject"], "Contract review");
    assert_eq!(
        payload["message"]["link"],
        format!("rmail://message/{message_id}")
    );
    assert_eq!(
        payload["message"]["summary"],
        "Legal wants redlines back. They sign Monday."
    );
    assert_eq!(
        payload["message"]["action_items"],
        serde_json::json!(["return redlines"])
    );
    assert!(
        payload["message"]["body"].is_null(),
        "this destination was not registered for bodies: {}",
        with_payload[0].payload
    );
    assert!(
        !with_payload[0].payload.contains("confidential"),
        "the body must not be in the payload at all"
    );

    // Replay re-arms it.
    let replayed = client
        .replay_delivery(ReplayDeliveryRequest {
            delivery_id: delivery.id,
        })
        .await
        .unwrap()
        .into_inner()
        .delivery
        .unwrap();
    assert_eq!(replayed.id, delivery.id);
    assert_eq!(replayed.state, WebhookDeliveryState::Pending as i32);
    assert_eq!(replayed.attempts, 0);
    assert!(
        replayed.payload.is_empty(),
        "a replay's answer is `it is queued again`, not an echo of the mail"
    );

    // Remove takes the history with it.
    assert!(
        client
            .remove(RemoveWebhookRequest {
                name: "eng-alerts".to_owned(),
            })
            .await
            .unwrap()
            .into_inner()
            .removed
    );
    assert!(client
        .list(ListWebhooksRequest { reveal_url: false })
        .await
        .unwrap()
        .into_inner()
        .destinations
        .is_empty());
    assert!(client
        .list_deliveries(ListDeliveriesRequest {
            destination: String::new(),
            limit: 10,
            include_payload: false,
        })
        .await
        .unwrap()
        .into_inner()
        .deliveries
        .is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn a_body_destination_is_the_only_one_that_gets_a_body() {
    let server = TestServer::start().await;
    let message_id = server.message("Contract review").await;
    let mut client = server.client().await;
    client
        .register(RegisterWebhookRequest {
            include_body: true,
            events: Vec::new(),
            ..register("tickets", "https://tickets.example.com/in")
        })
        .await
        .unwrap();
    let delivery = client
        .forward(ForwardMessageRequest {
            message_id,
            destination: "tickets".to_owned(),
        })
        .await
        .unwrap()
        .into_inner()
        .delivery
        .unwrap();
    let listed = client
        .list_deliveries(ListDeliveriesRequest {
            destination: "tickets".to_owned(),
            limit: 10,
            include_payload: true,
        })
        .await
        .unwrap()
        .into_inner()
        .deliveries;
    assert_eq!(listed[0].id, delivery.id);
    let payload: serde_json::Value = serde_json::from_str(&listed[0].payload).unwrap();
    assert_eq!(payload["message"]["body"], "the confidential body");
    server.shutdown().await;
}

#[tokio::test]
async fn the_url_policy_and_the_name_rule_map_onto_the_right_codes() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Plaintext off loopback.
    let status = client
        .register(register("plain", "http://hooks.example.com/x"))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("https"), "{}", status.message());

    // A scheme this daemon does not POST to.
    assert_eq!(
        client
            .register(register("weird", "ftp://files.example.com/x"))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );

    // Userinfo in the URL.
    let status = client
        .register(register("creds", "https://u:hunter2@hooks.example.com/x"))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        !status.message().contains("hunter2"),
        "the refusal must not echo the credential"
    );

    // A signing source with nowhere to look.
    assert_eq!(
        client
            .register(RegisterWebhookRequest {
                secret_source: WebhookSecretSource::Env as i32,
                secret_reference: String::new(),
                ..register("keyless", "https://hooks.example.com/x")
            })
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );

    // An event value from a future client is refused rather than silently
    // dropped — a destination that subscribes to less than the caller asked
    // for is how somebody finds out six months later their alerts never fired.
    assert_eq!(
        client
            .register(RegisterWebhookRequest {
                events: vec![9999],
                ..register("future", "https://hooks.example.com/x")
            })
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );

    // Nothing above left a row behind.
    assert!(client
        .list(ListWebhooksRequest { reveal_url: false })
        .await
        .unwrap()
        .into_inner()
        .destinations
        .is_empty());

    // A duplicate name.
    client
        .register(register("dup", "https://hooks.example.com/x"))
        .await
        .unwrap();
    assert_eq!(
        client
            .register(register("dup", "https://hooks.example.com/y"))
            .await
            .unwrap_err()
            .code(),
        Code::AlreadyExists
    );

    // Loopback is the documented plaintext exemption, and it works.
    client
        .register(register("local", "http://127.0.0.1:9/x"))
        .await
        .unwrap();

    server.shutdown().await;
}

#[tokio::test]
async fn forward_refuses_what_it_should_and_never_takes_a_url() {
    let server = TestServer::start().await;
    let message_id = server.message("x").await;
    let mut client = server.client().await;

    // No such destination — and note there is no request field that would let
    // a caller supply a URL instead, which is the structural half of this.
    assert_eq!(
        client
            .forward(ForwardMessageRequest {
                message_id,
                destination: "nope".to_owned(),
            })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    // A URL where a name goes is just an unknown name.
    assert_eq!(
        client
            .forward(ForwardMessageRequest {
                message_id,
                destination: "https://attacker.example/collect".to_owned(),
            })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );

    client
        .register(RegisterWebhookRequest {
            disabled: true,
            ..register("off", "https://hooks.example.com/x")
        })
        .await
        .unwrap();
    assert_eq!(
        client
            .forward(ForwardMessageRequest {
                message_id,
                destination: "off".to_owned(),
            })
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );

    // An unknown message, against an enabled destination.
    client
        .register(register("on", "https://hooks.example.com/y"))
        .await
        .unwrap();
    assert_eq!(
        client
            .forward(ForwardMessageRequest {
                message_id: 999_999,
                destination: "on".to_owned(),
            })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        client
            .forward(ForwardMessageRequest {
                message_id: 0,
                destination: "on".to_owned(),
            })
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );

    // Nothing was queued by any of the above.
    assert!(client
        .list_deliveries(ListDeliveriesRequest {
            destination: String::new(),
            limit: 100,
            include_payload: false,
        })
        .await
        .unwrap()
        .into_inner()
        .deliveries
        .is_empty());

    server.shutdown().await;
}

/// A webhook URL is frequently the credential itself, so the routine listing
/// hands back its authority and nothing more. The full URL is available, but
/// only to a caller that asked for it in so many words.
#[tokio::test]
async fn list_redacts_the_destination_url_unless_it_is_asked_for() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let url = "https://hooks.slack.example/services/T0000/B0000/XXXXSECRETXXXX";
    let registered = client
        .register(register("eng-alerts", url))
        .await
        .unwrap()
        .into_inner()
        .destination
        .unwrap();
    assert_eq!(
        registered.url, url,
        "Register echoes back the URL the caller just supplied"
    );

    let listed = client
        .list(ListWebhooksRequest { reveal_url: false })
        .await
        .unwrap()
        .into_inner()
        .destinations;
    assert_eq!(listed[0].url, "https://hooks.slack.example");
    assert!(
        !listed[0].url.contains("XXXXSECRETXXXX"),
        "the routine listing must not carry the credential"
    );

    let revealed = client
        .list(ListWebhooksRequest { reveal_url: true })
        .await
        .unwrap()
        .into_inner()
        .destinations;
    assert_eq!(revealed[0].url, url);

    server.shutdown().await;
}

#[tokio::test]
async fn set_enabled_round_trips_and_is_not_found_for_an_unknown_name() {
    let server = TestServer::start().await;
    let message_id = server.message("x").await;
    let mut client = server.client().await;
    client
        .register(register("alerts", "https://hooks.example.com/x"))
        .await
        .unwrap();

    let disabled = client
        .set_enabled(SetWebhookEnabledRequest {
            name: "alerts".to_owned(),
            enabled: false,
        })
        .await
        .unwrap()
        .into_inner()
        .destination
        .unwrap();
    assert!(!disabled.enabled);
    // A disabled destination refuses a forward rather than queueing one that
    // will never go out.
    assert_eq!(
        client
            .forward(ForwardMessageRequest {
                message_id,
                destination: "alerts".to_owned(),
            })
            .await
            .unwrap_err()
            .code(),
        Code::FailedPrecondition
    );

    let enabled = client
        .set_enabled(SetWebhookEnabledRequest {
            name: "alerts".to_owned(),
            enabled: true,
        })
        .await
        .unwrap()
        .into_inner()
        .destination
        .unwrap();
    assert!(enabled.enabled);
    client
        .forward(ForwardMessageRequest {
            message_id,
            destination: "alerts".to_owned(),
        })
        .await
        .unwrap();

    assert_eq!(
        client
            .set_enabled(SetWebhookEnabledRequest {
                name: "nope".to_owned(),
                enabled: true,
            })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    server.shutdown().await;
}

/// A forward on a daemon with no dispatcher is queued, and says so.
///
/// The alternative — reporting success and letting the CLI print "it is sent
/// on the dispatcher's next tick" — is a lie on the default configuration,
/// which is exactly the state an operator is in the first time they try this.
#[tokio::test]
async fn forward_reports_that_this_daemon_is_not_running_a_dispatcher() {
    let server = TestServer::start().await;
    let message_id = server.message("x").await;
    let mut client = server.client().await;
    client
        .register(register("alerts", "https://hooks.example.com/x"))
        .await
        .unwrap();
    let response = client
        .forward(ForwardMessageRequest {
            message_id,
            destination: "alerts".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        !response.dispatcher_running,
        "webhooks.enabled is false on this daemon and the answer must say so"
    );
    assert_eq!(
        response.delivery.unwrap().state,
        WebhookDeliveryState::Pending as i32
    );
    server.shutdown().await;
}

#[tokio::test]
async fn replaying_an_unknown_delivery_is_not_found() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    assert_eq!(
        client
            .replay_delivery(ReplayDeliveryRequest {
                delivery_id: 12_345,
            })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    server.shutdown().await;
}
