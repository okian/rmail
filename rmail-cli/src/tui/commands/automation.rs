//! The automation and notification verbs (task 98): what leaves this machine,
//! what runs on it when mail arrives, and what interrupts the person using it.
//!
//! # Two verbs here write no request at all
//!
//! `:hook add` and `:notify set` render a TOML block instead of sending
//! anything. That is not a gap: `HookService` has no Create and
//! `NotificationService` has no SetThreshold, both deliberately, and both protos
//! give the same reason — a setting that lives in the operator's config file must
//! not also live in a database the service would then have to keep in sync with
//! it. `tui::config_block`'s module docs carry the rest of that argument,
//! including why `mail hook add` is right to edit the file and a session holding
//! it open is not.
//!
//! # `:webhook` is the only outbound-network surface for mail content
//!
//! Everything else in this vocabulary stays on the machine or goes to a model
//! provider under the AI privacy firewall. These verbs POST the operator's own
//! mail to a third party, which is why `--include-body` is spelled out at the
//! destination rather than per request, why a signing key is a *reference* here
//! exactly as an account password is, and why the listing hides each
//! destination's URL unless asked: a webhook URL is frequently the credential
//! itself.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;

use super::{first, flag, nth, switch, Answer, Request, Target};
use crate::tui::config_block::{ConfigBlock, ReadOnlyReason};
use crate::tui::model::Cmd;
use crate::tui::report::ReportColumn;

/// How a destination's payload is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Template {
    /// A JSON document with the facts as named fields.
    Generic,
    /// Slack's incoming-webhook shape.
    Slack,
}

impl Template {
    /// The rendering `text` names, or `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "generic" => Some(Self::Generic),
            "slack" => Some(Self::Slack),
            _ => None,
        }
    }
}

/// Where a destination's HMAC signing key is resolved from — a reference, never
/// the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Secret {
    /// The name of an environment variable holding it.
    Env(String),
    /// A command whose stdout is it.
    Command(String),
    /// A macOS Keychain service holding it.
    Keychain(String),
}

/// The event vocabulary a hook or a webhook subscribes to.
///
/// One list for both, because the protos deliberately share it — "a webhook and
/// a hook subscribe to the same things happening, and two spellings would mean an
/// operator has to know which surface they are on". Wire strings rather than an
/// enum here for the same reason `InjectionDetection.kind` is a string: the
/// refusal that names them is generated from this list, so the list and the
/// message cannot disagree.
pub const EVENTS: [&str; 5] = [
    "on_new_message",
    "on_label",
    "on_move",
    "on_rule_match",
    "on_sync_error",
];

/// The tiers a notification threshold can name.
///
/// The proto is explicit that a value outside this ladder delivers *nothing* and
/// only warns at startup, so a typo in a rendered block would silently switch
/// notifications off. Refused here instead, where the line that named it is still
/// on screen.
pub const TIERS: [&str; 4] = ["low", "normal", "high", "critical"];

/// The columns `:webhook list` draws.
fn destination_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("name", 16),
        ReportColumn::new("url", 30),
        ReportColumn::new("state", 9),
        ReportColumn::new("events", 22),
        ReportColumn::new("payload", 14),
        ReportColumn::new("signing", 18),
    ]
}

/// The columns the delivery queue draws.
fn delivery_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("id", 6),
        ReportColumn::new("destination", 14),
        ReportColumn::new("event", 16),
        ReportColumn::new("state", 10),
        ReportColumn::new("attempts", 9),
        ReportColumn::new("last", 24),
    ]
}

/// The columns every field-shaped answer here draws.
fn field_columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("what", 16),
        ReportColumn::new("value", 58),
    ]
}

/// The automation verbs' answers.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn answer(invocation: &Invocation, target: &Target, generation: u64) -> Option<Answer> {
    let verb = invocation.verb.join(" ");
    Some(match verb.as_str() {
        "webhook list" => Request::rows(
            Cmd::WebhookList {
                generation,
                // A webhook URL is frequently the credential itself — anyone
                // holding a Slack incoming-webhook URL can post to that channel
                // — so the routine listing shows the authority and nothing else
                // unless this is asked for.
                reveal_url: switch(invocation, "reveal-url"),
            },
            "webhooks — where mail leaves this machine",
            destination_columns(),
        ),
        "webhook add" => {
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "name it — :webhook add eng-alerts https://hooks.example.com/…".to_owned(),
                ));
            };
            let Some(url) = nth(invocation, 1) else {
                return Some(Answer::Refused(
                    "and where it POSTs to — https, or plaintext only on loopback".to_owned(),
                ));
            };
            let template = match flag(invocation, "template") {
                None => Template::Generic,
                Some(text) => match Template::parse(text) {
                    Some(template) => template,
                    None => {
                        return Some(Answer::Refused(format!(
                            "--template {text:?}: generic or slack"
                        )))
                    }
                },
            };
            let events = match events(invocation) {
                Ok(events) => events,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let secret = match secret(invocation) {
                Ok(secret) => secret,
                Err(why) => return Some(Answer::Refused(why)),
            };
            let max_attempts = match count(invocation, "max-attempts") {
                Ok(max_attempts) => max_attempts,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::WebhookAdd {
                    generation,
                    name: name.clone(),
                    url,
                    template,
                    events,
                    // Off unless asked, and asked for at the *destination*: a
                    // caller cannot request more of a message than the
                    // destination was registered to receive.
                    include_body: switch(invocation, "include-body"),
                    disabled: switch(invocation, "disabled"),
                    secret,
                    max_attempts,
                },
                &format!("webhook add {name}"),
                destination_columns(),
            )
        }
        "webhook rm" => {
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "which destination — :webhook list has the names".to_owned(),
                ));
            };
            // The one automation verb that asks. It deletes the destination
            // *and its delivery history*, which is the record of what left this
            // machine — expensive and impossible to undo, which is the judgement
            // `Request::confirm` exists for. `:webhook disable` is the reversible
            // answer and the refusal points at it.
            Answer::Rows(Box::new(Request {
                cmd: Cmd::WebhookRemove { name: name.clone() },
                title: format!("webhook rm {name}"),
                columns: field_columns(),
                confirm: Some(format!(
                    "forget {name} and its delivery history? :webhook disable stops it \
                     reversibly [y/N]"
                )),
                once: false,
            }))
        }
        "webhook enable" | "webhook disable" => {
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "which destination — :webhook list has the names".to_owned(),
                ));
            };
            let enabled = verb == "webhook enable";
            Request::rows(
                Cmd::WebhookEnabled {
                    generation,
                    name: name.clone(),
                    enabled,
                },
                &format!("{verb} {name}"),
                destination_columns(),
            )
        }
        "webhook deliveries" => {
            let limit = match count(invocation, "limit") {
                Ok(limit) => limit,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::WebhookDeliveries {
                    generation,
                    destination: flag(invocation, "destination").map(str::to_owned),
                    limit,
                    // Off by default: a queue listing is frequently pasted into
                    // a ticket, and the payload is the mail content the rest of
                    // this view deliberately does not restate.
                    show_payload: switch(invocation, "show-payload"),
                },
                "webhook deliveries — newest first",
                delivery_columns(),
            )
        }
        "webhook replay" => {
            let Some(delivery_id) = id(invocation) else {
                return Some(Answer::Refused(
                    "which delivery — :webhook deliveries has the ids".to_owned(),
                ));
            };
            Request::rows(
                Cmd::WebhookReplay {
                    generation,
                    delivery_id,
                },
                &format!("webhook replay {delivery_id}"),
                delivery_columns(),
            )
        }
        "forward" => {
            let Some(destination) = flag(invocation, "to") else {
                return Some(Answer::Refused(
                    "--to names a destination — :webhook list has them".to_owned(),
                ));
            };
            let Some(message_id) = id(invocation).or(target.message_id) else {
                return Some(Answer::Refused("no message selected".to_owned()));
            };
            Request::rows(
                Cmd::Forward {
                    generation,
                    message_id,
                    destination: destination.to_owned(),
                },
                &format!("forward {message_id} to {destination}"),
                delivery_columns(),
            )
        }
        "hook list" => Request::rows(
            Cmd::HookList { generation },
            "hooks — what runs on this machine when mail arrives",
            vec![
                ReportColumn::new("hook", 16),
                ReportColumn::new("event", 16),
                ReportColumn::new("state", 9),
                ReportColumn::new("timeout", 9),
                ReportColumn::new("command", 34),
            ],
        ),
        "hook test" => {
            let Some(name) = first(invocation) else {
                return Some(Answer::Refused(
                    "which hook — :hook list has the names".to_owned(),
                ));
            };
            Request::rows(
                Cmd::HookTest {
                    generation,
                    name: name.clone(),
                    // The daemon checks it parses as JSON and never interpolates
                    // it into the command — it only ever reaches the hook's
                    // stdin. Passed through unread for that reason.
                    event_json: flag(invocation, "event-json").map(str::to_owned),
                },
                &format!("hook test {name}"),
                field_columns(),
            )
        }
        "hook add" => match hook_block(invocation) {
            Ok(block) => Answer::Block(Box::new(block)),
            Err(why) => Answer::Refused(why),
        },
        "notify list" => {
            let since = match count(invocation, "since") {
                Ok(since) => since,
                Err(why) => return Some(Answer::Refused(why)),
            };
            Request::rows(
                Cmd::NotifyAlerts {
                    generation,
                    // Absent means "only what fires from now on", which is what
                    // a terminal wants; a value replays everything after it
                    // first. The proto spells the difference with an optional
                    // field precisely because a plain integer could not.
                    since_id: since,
                },
                "notify — alerts as they fire",
                vec![
                    ReportColumn::new("when", 17),
                    ReportColumn::new("tier", 9),
                    ReportColumn::new("account", 14),
                    ReportColumn::new("from", 22),
                    ReportColumn::new("subject", 26),
                    ReportColumn::new("why", 30),
                ],
            )
        }
        "notify score" => {
            let Some(message_id) = id(invocation).or(target.message_id) else {
                return Some(Answer::Refused("no message selected".to_owned()));
            };
            Request::rows(
                Cmd::NotifyScore {
                    generation,
                    message_id,
                },
                &format!("notify score {message_id}"),
                field_columns(),
            )
        }
        "notify set" => match notify_block(invocation) {
            Ok(block) => Answer::Block(Box::new(block)),
            Err(why) => Answer::Refused(why),
        },
        _ => return None,
    })
}

/// The first positional as a row id, if it is one.
fn id(invocation: &Invocation) -> Option<i64> {
    invocation.positionals.first()?.parse().ok()
}

/// A whole-number flag, if the line carried one.
///
/// # Errors
///
/// A message naming the offending flag and value.
fn count(invocation: &Invocation, name: &str) -> Result<Option<i64>, String> {
    match flag(invocation, name) {
        None => Ok(None),
        Some(text) => text
            .parse::<i64>()
            .ok()
            .filter(|value| *value >= 0)
            .map(Some)
            .ok_or_else(|| format!("--{name} {text:?}: a whole number, at least zero")),
    }
}

/// The events a line subscribed to, as canonical wire strings.
///
/// Both spellings of `--events`, comma-separated and repeated, for the reason
/// `--scope` takes both: `mail webhook add --events a,b` is accepted there.
///
/// # Errors
///
/// A message naming every event this build knows, when one is not among them.
fn events(invocation: &Invocation) -> Result<Vec<String>, String> {
    let mut events = Vec::new();
    for name in invocation
        .flags
        .iter()
        .filter(|flag| flag.name == "events")
        .filter_map(|flag| flag.value.as_deref())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if !EVENTS.contains(&name) {
            return Err(format!("{name:?}: one of {}", EVENTS.join(", ")));
        }
        if !events.iter().any(|kept| kept == name) {
            events.push(name.to_owned());
        }
    }
    Ok(events)
}

/// The signing-key *reference* a line carried, if it carried one.
///
/// At most one, for the reason an account's credential is at most one: the wire
/// field is a single source, and a request naming two would have one of them
/// silently dropped.
///
/// # Errors
///
/// A message naming the flags, when more than one was given.
fn secret(invocation: &Invocation) -> Result<Option<Secret>, String> {
    let mut found: Vec<(&str, Secret)> = Vec::new();
    for (name, wrap) in [
        ("secret-env", Secret::Env as fn(String) -> _),
        ("secret-command", Secret::Command),
        ("secret-keychain", Secret::Keychain),
    ] {
        if let Some(value) = flag(invocation, name) {
            found.push((name, wrap(value.to_owned())));
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop().map(|(_, secret)| secret)),
        _ => Err(format!(
            "one signing key at a time — {} were given",
            found
                .iter()
                .map(|(name, _)| format!("--{name}"))
                .collect::<Vec<_>>()
                .join(" and ")
        )),
    }
}

/// The `[[hooks.hooks]]` block a `:hook add` line describes.
///
/// Validated as strictly as `mail hook add` validates it — the event, the name,
/// the command, the timeout — because the block is going into the operator's
/// config file and a daemon that refuses to start is a worse outcome than a
/// refusal here.
///
/// # Errors
///
/// A message naming what is missing or wrong.
fn hook_block(invocation: &Invocation) -> Result<ConfigBlock, String> {
    use rmail_core::config::toml_string;

    let Some(event) = first(invocation) else {
        return Err(format!("which event — one of {}", EVENTS.join(", ")));
    };
    if !EVENTS.contains(&event.as_str()) {
        return Err(format!("{event:?}: one of {}", EVENTS.join(", ")));
    }
    let Some(name) = flag(invocation, "name") else {
        return Err("--name is what :hook list and :hook test address it by".to_owned());
    };
    let Some(command) = flag(invocation, "command") else {
        return Err("--command is the program to run; --arg adds one argument".to_owned());
    };
    if let Some(timeout) = flag(invocation, "timeout") {
        rmail_core::config::parse_human_duration(timeout)
            .map_err(|error| format!("--timeout {timeout:?}: {error}"))?;
    }
    let args: Vec<&str> = invocation
        .flags
        .iter()
        .filter(|flag| flag.name == "arg")
        .filter_map(|flag| flag.value.as_deref())
        .collect();

    let mut toml = String::from("[[hooks.hooks]]\n");
    toml.push_str(&format!("name = {}\n", toml_string(name)));
    toml.push_str(&format!("event = {}\n", toml_string(&event)));
    toml.push_str(&format!("command = {}\n", toml_string(command)));
    if !args.is_empty() {
        let items: Vec<String> = args.iter().map(|arg| toml_string(arg)).collect();
        toml.push_str(&format!("args = [{}]\n", items.join(", ")));
    }
    if switch(invocation, "disabled") {
        toml.push_str("enabled = false\n");
    }
    if let Some(timeout) = flag(invocation, "timeout") {
        toml.push_str(&format!("timeout = {}\n", toml_string(timeout)));
    }
    Ok(ConfigBlock::new(
        format!("the hook {name}"),
        toml,
        rmail_core::config_path_from_env(),
        ReadOnlyReason::ConfigFileOnly,
        "rmaild loads hooks at startup — restart it for this one to fire",
    ))
}

/// The `[notify]` block a `:notify set` line describes.
///
/// # Errors
///
/// A message naming what is wrong, or saying nothing was asked for.
fn notify_block(invocation: &Invocation) -> Result<ConfigBlock, String> {
    use rmail_core::config::toml_string;

    let mut toml = String::from("[notify]\n");
    let mut asked = false;
    if let Some(threshold) = flag(invocation, "threshold") {
        if !TIERS.contains(&threshold) {
            // A tier outside the ladder delivers *nothing* and only warns at
            // startup, so a typo pasted into the config file is notifications
            // silently switched off. Refused here, where the line is still on
            // screen.
            return Err(format!(
                "--threshold {threshold:?}: one of {}",
                TIERS.join(", ")
            ));
        }
        toml.push_str(&format!("threshold = {}\n", toml_string(threshold)));
        asked = true;
    }
    for (on, off, key) in [
        ("enabled", "disabled", "enabled"),
        ("subject", "no-subject", "include_subject"),
        ("reason", "no-reason", "include_reason"),
    ] {
        match (switch(invocation, on), switch(invocation, off)) {
            (true, true) => return Err(format!("--{on} and --{off} cannot both be given")),
            (true, false) => {
                toml.push_str(&format!("{key} = true\n"));
                asked = true;
            }
            (false, true) => {
                toml.push_str(&format!("{key} = false\n"));
                asked = true;
            }
            (false, false) => {}
        }
    }
    if !asked {
        // A `[notify]` header with nothing under it would be a block that
        // changes nothing, presented as a block that changes something.
        return Err(
            "say what to set — --threshold=high, --enabled/--disabled, --subject/--no-subject, \
             --reason/--no-reason"
                .to_owned(),
        );
    }
    Ok(ConfigBlock::new(
        "the [notify] block",
        toml,
        rmail_core::config_path_from_env(),
        ReadOnlyReason::ConfigFileOnly,
        "rmaild reads [notify] at startup — restart it to apply this",
    ))
}
