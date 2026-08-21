//! Task 98's automation verbs: the outbound-network surface, the local commands,
//! the live alert feed — and the two verbs that write nothing at all.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_core::keymap::Key;
use rmail_proto::v1::{
    Alert, ForwardMessageResponse, HookEvent, HookInfo, ListHooksResponse, NotificationState,
    NotificationTier, ScoreMessageResponse, TestHookResponse, WebhookDelivery,
    WebhookDeliveryState, WebhookDestination, WebhookEvent, WebhookSecretSource,
};

use super::*;
use crate::tui::config_block::ReadOnlyReason;
use crate::tui::model::{
    update, wire, Account, Folder, MessageRow, Model, Msg, Overlay, ReportEvent,
};
use crate::tui::report::{ReportFill, ReportTone};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn invocation(line: &str) -> Invocation {
    match command::parse(line) {
        Ok(Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
    }
}

fn screen() -> Target {
    Target {
        account_id: 7,
        mailbox_id: Some(1),
        message_id: Some(10),
        selection: vec![10, 11],
        rule_draft: None,
    }
}

fn asked(line: &str, target: &Target) -> Answer {
    match answer(&invocation(line), target, 5) {
        Some(answer) => answer,
        None => panic!("{line:?} has no answer"),
    }
}

fn request_on(line: &str, target: &Target) -> Request {
    match asked(line, target) {
        Answer::Rows(request) | Answer::Fact(request) => *request,
        other => panic!("{line:?} is not a request: {other:?}"),
    }
}

fn request(line: &str) -> Request {
    request_on(line, &screen())
}

fn block(line: &str) -> ConfigBlock {
    match asked(line, &screen()) {
        Answer::Block(block) => *block,
        other => panic!("{line:?} is not a block: {other:?}"),
    }
}

fn refusal(line: &str) -> String {
    match asked(line, &screen()) {
        Answer::Refused(why) => why,
        other => panic!("{line:?} was not refused: {other:?}"),
    }
}

fn loaded() -> Model {
    let mut model = Model::new();
    model.accounts = vec![Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    }];
    model.account = model.accounts.first().cloned();
    model.folders = vec![Folder {
        id: 1,
        name: "INBOX".to_owned(),
        message_count: 2,
    }];
    model.open_folder = Some(1);
    model.messages = (10..12)
        .map(|id| MessageRow {
            id,
            subject: format!("subject {id}"),
            from: "Alice".to_owned(),
            from_addr: Some("alice@example.com".to_owned()),
            date: Some(1_700_000_000 + id),
            flags: Vec::new(),
            has_attachments: false,
            has_note: false,
            to: None,
            tags: Vec::new(),
            ai: None,
        })
        .collect();
    model
}

fn run(model: &mut Model, line: &str) -> Vec<Cmd> {
    update(model, Msg::Key(Key::Char(':')));
    for c in line.chars() {
        update(model, Msg::Key(Key::Char(c)));
    }
    update(model, Msg::Key(Key::Enter))
        .into_iter()
        .filter(|cmd| !matches!(cmd, Cmd::SaveHistory { .. }))
        .collect()
}

fn destination(enabled: bool, include_body: bool) -> WebhookDestination {
    WebhookDestination {
        id: 1,
        name: "eng-alerts".to_owned(),
        url: "https://hooks.example.com".to_owned(),
        template: 1,
        events: vec![WebhookEvent::OnNewMessage as i32],
        include_body,
        enabled,
        secret_source: WebhookSecretSource::Env as i32,
        secret_reference: "RMAIL_HOOK_KEY".to_owned(),
        max_attempts: 5,
    }
}

fn delivery(state: WebhookDeliveryState) -> WebhookDelivery {
    WebhookDelivery {
        id: 9,
        destination_id: 1,
        destination_name: "eng-alerts".to_owned(),
        event_key: "event:12".to_owned(),
        event: "on_new_message".to_owned(),
        message_id: 10,
        state: state as i32,
        attempts: 5,
        max_attempts: 5,
        next_attempt_at: 0,
        last_status: 500,
        last_error: "HTTP 500".to_owned(),
        created_at: 1_700_000_000,
        delivered_at: 0,
        payload: String::new(),
    }
}

// ---------------------------------------------------------------------------
// webhooks: the outbound surface
// ---------------------------------------------------------------------------

#[test]
fn a_destination_url_is_hidden_unless_asked_for() {
    // A webhook URL is frequently the credential itself, so the routine listing
    // shows the authority and nothing else — including when the same tool is
    // projected over MCP and its result lands in a model's context.
    assert_eq!(
        request("webhook list").cmd,
        Cmd::WebhookList {
            generation: 5,
            reveal_url: false,
        }
    );
    let Cmd::WebhookList { reveal_url, .. } = request("webhook list --reveal-url").cmd else {
        panic!("expected a listing");
    };
    assert!(reveal_url);
}

#[test]
fn registering_needs_a_name_and_a_url() {
    assert!(refusal("webhook add").contains("name it"));
    assert!(refusal("webhook add eng-alerts").contains("POSTs to"));
    assert_eq!(
        request("webhook add eng-alerts https://hooks.example.com/x").cmd,
        Cmd::WebhookAdd {
            generation: 5,
            name: "eng-alerts".to_owned(),
            url: "https://hooks.example.com/x".to_owned(),
            template: Template::Generic,
            events: Vec::new(),
            include_body: false,
            disabled: false,
            secret: None,
            max_attempts: None,
        }
    );
}

#[test]
fn the_body_entitlement_is_off_unless_it_is_asked_for() {
    // The default payload is a notification, not the mail: sender, subject, a
    // deep link. Turning this on ships the body text itself to a third party on
    // every matching message, which is why it is a property of the destination
    // and why nothing infers it.
    let Cmd::WebhookAdd { include_body, .. } =
        request("webhook add x https://h.example.com --include-body").cmd
    else {
        panic!("expected a registration");
    };
    assert!(include_body);
}

#[test]
fn events_are_read_from_both_spellings_and_checked_against_the_vocabulary() {
    let Cmd::WebhookAdd { events, .. } = request(
        "webhook add x https://h.example.com --events=on_new_message,on_label --events=on_move",
    )
    .cmd
    else {
        panic!("expected a registration");
    };
    assert_eq!(
        events,
        vec![
            "on_new_message".to_owned(),
            "on_label".to_owned(),
            "on_move".to_owned(),
        ]
    );
    // Deduplicated: two subscriptions to one event is not two subscriptions.
    let Cmd::WebhookAdd { events, .. } =
        request("webhook add x https://h.example.com --events=on_move,on_move").cmd
    else {
        panic!("expected a registration");
    };
    assert_eq!(events, vec!["on_move".to_owned()]);

    let why = refusal("webhook add x https://h.example.com --events=on_delete");
    assert!(why.contains("on_new_message"), "{why}");
}

#[test]
fn every_event_this_grammar_accepts_has_a_wire_value() {
    // The answer table checks a name against `EVENTS` and the wire seam maps it;
    // a name in one and not the other would be a subscription accepted here and
    // silently dropped there.
    for name in EVENTS {
        assert!(wire::webhook_event(name).is_some(), "{name}");
    }
    assert!(wire::webhook_event("on_delete").is_none());
}

#[test]
fn two_signing_keys_are_refused_rather_than_one_being_dropped() {
    let why = refusal("webhook add x https://h.example.com --secret-env=A --secret-command=B");
    assert!(why.contains("one signing key at a time"), "{why}");
}

#[test]
fn a_template_outside_the_two_is_refused() {
    let why = refusal("webhook add x https://h.example.com --template=teams");
    assert!(why.contains("generic or slack"), "{why}");
    let Cmd::WebhookAdd { template, .. } =
        request("webhook add x https://h.example.com --template=slack").cmd
    else {
        panic!("expected a registration");
    };
    assert_eq!(template, Template::Slack);
}

#[test]
fn removing_asks_and_names_the_reversible_answer() {
    // It deletes the destination *and its delivery history*, which is the record
    // of what already left this machine. `disable` is the reversible answer, and
    // the question says so rather than leaving somebody to find out afterwards.
    let request = request("webhook rm eng-alerts");
    let prompt = request.confirm.expect("it asks");
    assert!(prompt.contains("delivery history"), "{prompt}");
    assert!(prompt.contains(":webhook disable"), "{prompt}");
    assert_eq!(
        request.cmd,
        Cmd::WebhookRemove {
            name: "eng-alerts".to_owned(),
        }
    );
}

#[test]
fn enable_and_disable_are_one_rpc_in_two_directions() {
    for (line, enabled) in [("webhook enable x", true), ("webhook disable x", false)] {
        assert_eq!(
            request(line).cmd,
            Cmd::WebhookEnabled {
                generation: 5,
                name: "x".to_owned(),
                enabled,
            },
            "{line}"
        );
        // Neither asks: disabling is reversible by enabling, and enabling is
        // reversible by disabling.
        assert!(request(line).confirm.is_none(), "{line} asks");
    }
}

#[test]
fn a_destination_entitled_to_bodies_is_the_one_row_drawn_as_a_warning() {
    // It ships the mail itself, redacted, to a third party on every matching
    // message. That is the configuration worth finding at a glance.
    assert_eq!(
        wire::destination_row(&destination(true, true)).tone,
        ReportTone::Warn
    );
    assert_eq!(
        wire::destination_row(&destination(true, false)).tone,
        ReportTone::Ok
    );
    assert_eq!(
        wire::destination_row(&destination(false, true)).tone,
        ReportTone::Muted
    );
}

#[test]
fn a_destination_that_subscribes_to_nothing_says_what_it_is_for() {
    // It is a real and useful configuration — it receives an explicit
    // `:forward` and no firehose — and a blank cell would read as a rendering
    // fault instead.
    let mut destination = destination(true, false);
    destination.events.clear();
    let row = wire::destination_row(&destination);
    assert_eq!(row.cells[3], "forward only");
    // And an unsigned destination says so rather than implying a receiver can
    // verify something.
    destination.secret_source = WebhookSecretSource::Unspecified as i32;
    assert_eq!(wire::destination_row(&destination).cells[5], "unsigned");
}

// ---------------------------------------------------------------------------
// the delivery queue
// ---------------------------------------------------------------------------

#[test]
fn only_a_failed_delivery_offers_a_replay() {
    // Replay is the only way out of the terminal state, and deliberately
    // something a human does — which is exactly what a row action is. A pending
    // row offering it would be inviting a second POST of something still on its
    // way.
    let failed = wire::delivery_row(&delivery(WebhookDeliveryState::Failed));
    let replay = failed.on_enter.clone().expect("a failed row replays");
    assert_eq!(replay.verb, vec!["webhook", "replay"]);
    assert_eq!(replay.positionals, vec!["9".to_owned()]);
    // Not bang'd: replaying POSTs the same mail content to a third party again,
    // so task 90's gate asking first is the gate doing its job.
    assert!(!replay.bang);

    for state in [
        WebhookDeliveryState::Pending,
        WebhookDeliveryState::Delivered,
    ] {
        assert!(
            wire::delivery_row(&delivery(state)).on_enter.is_none(),
            "{state:?} offers a replay"
        );
    }
}

#[test]
fn a_peer_that_never_answered_is_not_reported_as_a_500() {
    // Different operational facts, and the proto keeps them apart on purpose:
    // `last_status` is 0 when nothing answered at all.
    let mut row = delivery(WebhookDeliveryState::Pending);
    row.last_status = 0;
    row.last_error = String::new();
    row.next_attempt_at = 0;
    assert_eq!(wire::delivery_row(&row).cells[5], "no answer yet");
}

#[test]
fn forwarding_says_queued_and_says_it_louder_with_no_dispatcher() {
    // A client that said "sent" on a daemon with `webhooks.enabled = false`
    // would be the lie the response's own `dispatcher_running` field exists to
    // prevent.
    let running = wire::forwarded(&ForwardMessageResponse {
        delivery: Some(delivery(WebhookDeliveryState::Pending)),
        dispatcher_running: true,
    });
    assert!(running.contains("queued"), "{running}");
    assert!(!running.contains("not sent"), "{running}");
    let stopped = wire::forwarded(&ForwardMessageResponse {
        delivery: Some(delivery(WebhookDeliveryState::Pending)),
        dispatcher_running: false,
    });
    assert!(stopped.contains("no dispatcher"), "{stopped}");
    assert!(stopped.contains("not sent"), "{stopped}");
}

#[test]
fn forwarding_needs_a_destination_and_a_message() {
    assert!(refusal("forward").contains("--to names a destination"));
    assert_eq!(
        request("forward --to=eng-alerts").cmd,
        Cmd::Forward {
            generation: 5,
            message_id: 10,
            destination: "eng-alerts".to_owned(),
        }
    );
    // An explicit id wins over the message on screen.
    let Cmd::Forward { message_id, .. } = request("forward 42 --to=eng-alerts").cmd else {
        panic!("expected a forward");
    };
    assert_eq!(message_id, 42);
    let no_message = Target {
        message_id: None,
        ..screen()
    };
    let why = match asked("forward --to=x", &no_message) {
        Answer::Refused(why) => why,
        other => panic!("{other:?}"),
    };
    assert!(why.contains("no message selected"), "{why}");
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

#[test]
fn testing_a_hook_needs_its_name_and_passes_the_payload_through_unread() {
    assert!(refusal("hook test").contains(":hook list"));
    assert_eq!(
        request("hook test notify").cmd,
        Cmd::HookTest {
            generation: 5,
            name: "notify".to_owned(),
            event_json: None,
        }
    );
    // Passed through verbatim: the daemon checks it parses as JSON and never
    // interpolates it into the command, so there is nothing for this client to
    // interpret.
    //
    // Quoted through `command::quoted`, which is how JSON reaches a `:` line at
    // all: the tokenizer treats a bare `"` as quote syntax and strips it, so
    // `--event-json={"a":1}` typed raw arrives as `{a:1}` — valid to the
    // tokenizer and not JSON any more.
    let payload = "{\"a\":1}";
    let line = format!("hook test notify --event-json={}", command::quoted(payload));
    let Cmd::HookTest { event_json, .. } = request(&line).cmd else {
        panic!("expected a test");
    };
    assert_eq!(event_json.as_deref(), Some(payload));
}

#[test]
fn a_hook_run_reports_five_outcomes_apart() {
    // They are five different operational facts, and collapsing any two would
    // send somebody looking in the wrong place.
    let base = TestHookResponse {
        timed_out: false,
        cancelled: false,
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
        duration_ms: 12,
    };
    let outcome = |response: &TestHookResponse| {
        let rows = wire::hook_test_rows(response);
        (rows[0].cells[1].clone(), rows[0].tone)
    };
    assert_eq!(outcome(&base), ("exit 0".to_owned(), ReportTone::Ok));
    assert_eq!(
        outcome(&TestHookResponse {
            exit_code: Some(1),
            ..base.clone()
        }),
        ("exit 1".to_owned(), ReportTone::Bad)
    );
    let (text, tone) = outcome(&TestHookResponse {
        exit_code: None,
        ..base.clone()
    });
    assert!(text.contains("could not be spawned"), "{text}");
    assert_eq!(tone, ReportTone::Bad);
    let (text, tone) = outcome(&TestHookResponse {
        timed_out: true,
        exit_code: None,
        ..base.clone()
    });
    assert!(text.contains("timeout"), "{text}");
    assert_eq!(tone, ReportTone::Bad);
    let (text, tone) = outcome(&TestHookResponse {
        cancelled: true,
        exit_code: None,
        ..base.clone()
    });
    assert!(text.contains("shut down"), "{text}");
    assert_eq!(tone, ReportTone::Warn);
}

#[test]
fn hook_output_is_drawn_a_line_at_a_time() {
    // It is output somebody is reading; folded into one cell it would be elided
    // at the column width.
    let rows = wire::hook_test_rows(&TestHookResponse {
        timed_out: false,
        cancelled: false,
        exit_code: Some(0),
        stdout: "first\nsecond\n".to_owned(),
        stderr: String::new(),
        duration_ms: 3,
    });
    let out: Vec<&str> = rows
        .iter()
        .filter(|row| row.cells[0] == "stdout")
        .map(|row| row.cells[1].as_str())
        .collect();
    assert_eq!(out, vec!["first", "second"]);
}

#[test]
fn a_disabled_hook_is_listed_and_drawn_dim() {
    let response = ListHooksResponse {
        hooks: vec![
            HookInfo {
                name: "notify".to_owned(),
                event: HookEvent::OnNewMessage as i32,
                command: "/bin/notify".to_owned(),
                args: vec!["--quiet".to_owned()],
                enabled: true,
                timeout_ms: 10_000,
            },
            HookInfo {
                name: "old".to_owned(),
                event: HookEvent::OnMove as i32,
                command: "/bin/old".to_owned(),
                args: Vec::new(),
                enabled: false,
                timeout_ms: 5_000,
            },
        ],
    };
    let rows = wire::hook_rows(&response);
    assert_eq!(rows[0].tone, ReportTone::Ok);
    assert_eq!(rows[0].cells[1], "on_new_message");
    assert_eq!(rows[0].cells[4], "/bin/notify --quiet");
    assert_eq!(rows[1].tone, ReportTone::Muted);
    assert_eq!(rows[1].cells[2], "disabled");
}

// ---------------------------------------------------------------------------
// the two verbs that write nothing
// ---------------------------------------------------------------------------

#[test]
fn hook_add_renders_the_block_and_sends_nothing() {
    let block = block("hook add on_new_message --name=notify --command=/bin/notify --arg=-q");
    assert_eq!(block.reason, ReadOnlyReason::ConfigFileOnly);
    assert_eq!(block.path, rmail_core::config_path_from_env());
    assert!(block.effect.contains("restart"), "{}", block.effect);
    assert_eq!(
        block.toml,
        "[[hooks.hooks]]\nname = \"notify\"\nevent = \"on_new_message\"\n\
         command = \"/bin/notify\"\nargs = [\"-q\"]\n"
    );
}

#[test]
fn hook_add_validates_everything_it_is_about_to_write() {
    // The block is going into the operator's config file, and a daemon that
    // refuses to start is a worse outcome than a refusal here.
    assert!(refusal("hook add").contains("on_new_message"));
    assert!(refusal("hook add on_delete --name=x --command=y").contains("on_new_message"));
    assert!(refusal("hook add on_move --command=y").contains("--name"));
    assert!(refusal("hook add on_move --name=x").contains("--command"));
    let why = refusal("hook add on_move --name=x --command=y --timeout=soon");
    assert!(why.contains("--timeout"), "{why}");
}

#[test]
fn a_value_with_a_quote_in_it_is_escaped_for_toml() {
    // The block is TOML, and an unescaped quote in a command would produce a
    // file that does not parse — which the operator would find out about at the
    // next daemon start, not here.
    let block = block("hook add on_move --name=x --command=\"say \\\"hi\\\"\"");
    assert!(
        block.toml.contains("command = \"say \\\"hi\\\"\""),
        "{}",
        block.toml
    );
}

#[test]
fn notify_set_renders_only_what_was_asked_for() {
    let block = block("notify set --threshold=high --no-subject");
    assert_eq!(block.reason, ReadOnlyReason::ConfigFileOnly);
    assert_eq!(
        block.toml,
        "[notify]\nthreshold = \"high\"\ninclude_subject = false\n"
    );
}

#[test]
fn a_threshold_outside_the_ladder_is_refused_rather_than_rendered() {
    // A tier outside it delivers *nothing* and only warns at daemon startup, so
    // a typo pasted into the config file is notifications silently switched off.
    let why = refusal("notify set --threshold=urgent");
    assert!(why.contains("low, normal, high, critical"), "{why}");
}

#[test]
fn a_switch_and_its_opposite_cannot_both_be_given() {
    // The block would have to say one of them, and picking silently is picking
    // for somebody who has just said they want both.
    for line in [
        "notify set --enabled --disabled",
        "notify set --subject --no-subject",
        "notify set --reason --no-reason",
    ] {
        let why = refusal(line);
        assert!(why.contains("cannot both be given"), "{line}: {why}");
    }
}

#[test]
fn notify_set_with_nothing_to_set_is_refused() {
    // A `[notify]` header with nothing under it is a block that changes nothing,
    // presented as a block that changes something.
    let why = refusal("notify set");
    assert!(why.contains("say what to set"), "{why}");
}

#[test]
fn a_block_opens_a_report_and_is_remembered_for_toml() {
    let mut model = loaded();
    let cmds = run(&mut model, "hook add on_move --name=x --command=/bin/y");
    assert!(cmds.is_empty(), "a block reaches no daemon: {cmds:?}");
    let Some(Overlay::Report(pane)) = model.overlay_top() else {
        panic!("expected a report");
    };
    // Complete on arrival: nothing is outstanding, so a border reading "asking…"
    // would describe a request that was never made.
    assert!(pane.complete);
    assert!(pane
        .rows
        .iter()
        .any(|row| row.cells[1] == "[[hooks.hooks]]"));
    // `r` refuses: the block is a pure function of the line, so re-running would
    // redraw exactly what is already there.
    let cmds = update(&mut model, Msg::Key(Key::Char('r')));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(model.status.contains("already showing"), "{}", model.status);
    // And it outlives the report, which is the whole reason it is session state.
    update(&mut model, Msg::Key(Key::Esc));
    let cmds = run(&mut model, "toml");
    assert!(
        matches!(cmds.first(), Some(Cmd::OpenText { extension, .. }) if extension == "toml"),
        "{cmds:?}"
    );
}

// ---------------------------------------------------------------------------
// notifications
// ---------------------------------------------------------------------------

#[test]
fn the_alert_feed_replays_only_when_asked_to() {
    // Absent means "only what fires from now on" — a terminal should not fill
    // with a week of history on every invocation — and a value replays
    // everything after it. The proto uses an optional field precisely because a
    // plain integer could not express both.
    assert_eq!(
        request("notify list").cmd,
        Cmd::NotifyAlerts {
            generation: 5,
            since_id: None,
        }
    );
    assert_eq!(
        request("notify list --since=0").cmd,
        Cmd::NotifyAlerts {
            generation: 5,
            // Zero is "replay the whole retained history", since alert ids start
            // at 1 — so it must survive as a value rather than being folded into
            // the absent case.
            since_id: Some(0),
        }
    );
    let why = refusal("notify list --since=soon");
    assert!(why.contains("--since"), "{why}");
}

#[test]
fn an_alert_is_drawn_by_how_loud_it_is() {
    let alert = |tier: NotificationTier| Alert {
        id: 1,
        message_id: 10,
        account: "personal".to_owned(),
        tier: tier as i32,
        reason: "someone is waiting".to_owned(),
        subject: Some("the thing".to_owned()),
        from: Some("Ada".to_owned()),
        delivered_at: 1_700_000_000,
    };
    assert_eq!(
        wire::alert_row(&alert(NotificationTier::Critical)).tone,
        ReportTone::Bad
    );
    assert_eq!(
        wire::alert_row(&alert(NotificationTier::High)).tone,
        ReportTone::Warn
    );
    assert_eq!(
        wire::alert_row(&alert(NotificationTier::Low)).tone,
        ReportTone::Muted
    );
    let row = wire::alert_row(&alert(NotificationTier::High));
    assert_eq!(row.cells[1], "high");
    assert_eq!(row.cells[5], "someone is waiting");
}

#[test]
fn the_live_feed_never_completes_and_a_closed_stream_is_a_failure() {
    // It is the live tail: a border that said "done" would be describing a feed
    // that is still listening. And a feed the daemon closed is worth saying,
    // because a pane that silently stopped can no longer tell anybody anything.
    let mut model = loaded();
    let cmds = run(&mut model, "notify list");
    let generation = match cmds.first() {
        Some(Cmd::NotifyAlerts { generation, .. }) => *generation,
        other => panic!("expected a feed: {other:?}"),
    };
    update(
        &mut model,
        Msg::Report {
            generation,
            event: ReportEvent::Frame {
                fill: ReportFill::Append,
                rows: vec![wire::alert_row(&Alert {
                    id: 1,
                    message_id: 10,
                    account: "personal".to_owned(),
                    tier: NotificationTier::High as i32,
                    reason: "waiting".to_owned(),
                    subject: None,
                    from: None,
                    delivered_at: 1_700_000_000,
                })],
                complete: false,
            },
        },
    );
    let Some(Overlay::Report(pane)) = model.overlay_top() else {
        panic!("expected a report");
    };
    assert_eq!(pane.rows.len(), 1);
    assert!(!pane.complete, "the feed is still listening");
}

#[test]
fn scoring_explains_a_silence_rather_than_only_naming_a_tier() {
    // The interesting answer is usually why nothing happened, so the threshold,
    // the account switch and the suppression reason are all rows.
    let rows = wire::score_rows(&ScoreMessageResponse {
        state: NotificationState::Suppressed as i32,
        tier: Some(NotificationTier::Normal as i32),
        reason: Some("a newsletter".to_owned()),
        suppressed_reason: "below_threshold".to_owned(),
        effective_threshold: "high".to_owned(),
        account_enabled: true,
        would_notify: false,
    });
    let cell = |what: &str| {
        rows.iter()
            .find(|row| row.cells[0] == what)
            .map(|row| row.cells[1].clone())
            .unwrap_or_else(|| panic!("no {what} row: {rows:?}"))
    };
    assert_eq!(cell("state"), "suppressed");
    assert_eq!(cell("tier"), "normal");
    assert_eq!(cell("suppressed"), "below_threshold");
    assert_eq!(cell("threshold"), "high");
    assert_eq!(cell("would notify"), "no");
}

#[test]
fn a_message_nobody_has_scored_says_so_rather_than_naming_a_tier() {
    let rows = wire::score_rows(&ScoreMessageResponse {
        state: NotificationState::Queued as i32,
        tier: None,
        reason: None,
        suppressed_reason: String::new(),
        effective_threshold: "high".to_owned(),
        account_enabled: true,
        would_notify: false,
    });
    assert!(rows[0].cells[1].contains("queued"), "{:?}", rows[0].cells);
    assert_eq!(rows[1].cells[1], "not scored yet");
}

#[test]
fn scoring_needs_a_message() {
    assert_eq!(
        request("notify score").cmd,
        Cmd::NotifyScore {
            generation: 5,
            message_id: 10,
        }
    );
    let no_message = Target {
        message_id: None,
        ..screen()
    };
    match asked("notify score", &no_message) {
        Answer::Refused(why) => assert!(why.contains("no message selected"), "{why}"),
        other => panic!("{other:?}"),
    }
}
