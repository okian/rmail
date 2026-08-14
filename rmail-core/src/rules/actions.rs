//! Firing a matched rule's actions — and describing what they *would* do,
//! for a dry run.
//!
//! # One action failing does not abandon the rest
//!
//! [`ActionRunner::apply`] never returns an `Err`. Every action produces an
//! [`ActionOutcome`] saying whether it landed and why not, and the runner
//! moves on to the next one. This is [`crate::hooks::run_hook`]'s contract
//! applied one level up, for the same reason: a rule naming a mailbox the
//! account does not have is an ordinary, expected misconfiguration, and
//! letting it swallow the `add_labels` that would have worked turns one typo
//! into silence across the whole rule.
//!
//! # Actions are at most once, never more
//!
//! [`super::RuleEngine`] claims `(rule, message)` in `rule_actions_fired`
//! *before* calling this module, and skips entirely when the claim is already
//! taken. That ordering — claim, then act — is deliberate and is the opposite
//! of what [`crate::smart_folder`] does for its own ledger.
//!
//! The two differ because the actions differ. A smart folder's actions are an
//! auto-tag (idempotent by construction) and a notification, so it acts first
//! and stamps second, accepting a duplicate on a crash in exchange for never
//! losing one. A rule's actions include `draft_reply` (a *new* draft each
//! time — a mailbox slowly filling with identical drafts is a bug a user
//! would report) and `run_hook` (an arbitrary process, run again). For that
//! set, at-most-once is the right trade: the cost of the crash window is one
//! rule's actions not running for one message, which the user can re-fire by
//! hand, versus an unbounded number of duplicated side effects they cannot
//! easily undo.
//!
//! # A move is last, because it removes the local row
//!
//! [`crate::mail::MailStore::move_message`] does not re-point
//! `messages.mailbox_id`; it issues the IMAP `MOVE` and then *deletes* the
//! local row, because the destination folder assigns a new UID that only its
//! next sync can learn (see that method's own docs). Two consequences shape
//! the order [`ActionRunner::apply`] fires things in:
//!
//! - **Labels and flags run first.** They address the message by its current
//!   mailbox and UID, which stop being valid the instant the move lands. Run
//!   first, an `imap`-synced tag's keyword `STORE` reaches the server while
//!   the message is still there and travels with it to the destination.
//! - **A local-only annotation applied by the same rule does not survive the
//!   move.** `message_tags` rows are keyed to the local message id and go
//!   with it. That is a property of how moves work in this codebase rather
//!   than something this module can fix, and it is why a rule that both
//!   `archive`s and `add_labels` should use a tag whose `sync_mode` reaches
//!   IMAP if the label is meant to outlive the move.
//!
//! # A dry run touches nothing
//!
//! [`ActionRunner::describe`] performs the same lookups `apply` does — does
//! the destination mailbox exist, is the named hook configured — so a dry run
//! reports the misconfiguration a real run would hit, but issues no
//! mutation, no IMAP round trip, no process spawn, and no event. The
//! `rules::tests::a_dry_run_makes_no_mutation_and_spawns_no_process` case
//! pins that against a mutator that errors on every call.

use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::compose::{DraftStore, Mailbox, NewDraft};
use crate::error::Error;
use crate::events::{EventKind, EventLog, NewEvent};
use crate::hooks::{self, ResolvedHook};
use crate::mail::MailStore;
use crate::rules::facts::MessageFacts;
use crate::rules::model::Actions;
use crate::rules::repo;
use crate::storage::Database;
use crate::tags::{TagSource, TagStore, Target};

/// The longest `draft_reply` body accepted into a draft. Rules are config,
/// not a composer; a body past this is a mistake.
const MAX_DRAFT_BODY: usize = 16 * 1024;

/// What one action did, or would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionOutcome {
    /// The action's name, matching the TOML key (`move_to`, `add_labels`, ...).
    pub action: String,
    /// Whether it succeeded. Always `false` on the dry-run path — see
    /// [`ActionOutcome::detail`], which says what *would* have happened.
    pub applied: bool,
    /// Human-readable detail: what it did, or why it could not.
    pub detail: String,
}

impl ActionOutcome {
    fn ok(action: &str, detail: impl Into<String>) -> Self {
        Self {
            action: action.to_owned(),
            applied: true,
            detail: detail.into(),
        }
    }

    fn failed(action: &str, detail: impl Into<String>) -> Self {
        Self {
            action: action.to_owned(),
            applied: false,
            detail: detail.into(),
        }
    }

    /// A dry-run entry: `applied` is `false` because nothing was applied,
    /// and `detail` says what a real run would do. Distinct from
    /// [`ActionOutcome::failed`] only in intent, which is why it exists as
    /// its own constructor rather than as a bare `failed` at every dry-run
    /// call site.
    fn planned(action: &str, detail: impl Into<String>) -> Self {
        Self::failed(action, detail)
    }

    /// An action the prompt-injection shield withheld: it was configured, the
    /// rule matched, and it did not run because the model's contribution to
    /// that match came from a message flagged as hostile.
    ///
    /// `pub(super)` so [`super::RuleEngine`] can build one without this
    /// module's private constructors leaking further. Reported per configured
    /// action rather than as one summary line — "your mail was not archived
    /// and no hook ran" is the useful sentence; "actions withheld" is not.
    pub(super) fn withheld(action: &str, detail: impl Into<String>) -> Self {
        Self::failed(action, detail)
    }
}

/// Everything a rule's actions need to reach.
///
/// Cheap to clone — every field is a handle or a small owned list.
#[derive(Debug, Clone)]
pub struct ActionRunner {
    db: Database,
    mail: MailStore,
    tags: TagStore,
    drafts: DraftStore,
    events: EventLog,
    /// Every configured hook, resolved once. A rule's `run_hook` names one of
    /// these; it can never name an arbitrary command, which is the property
    /// that keeps `crate::hooks`' "operator-authored argv only" invariant
    /// intact even though a *rule* is user-authored.
    hooks: Vec<ResolvedHook>,
    /// Shared with the hook dispatcher so rule-fired hooks and event-fired
    /// hooks together stay inside `hooks.max_concurrency`.
    hook_semaphore: Arc<Semaphore>,
    hook_max_output_bytes: usize,
    archive_mailbox: String,
}

impl ActionRunner {
    /// Build a runner.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        mail: MailStore,
        tags: TagStore,
        drafts: DraftStore,
        events: EventLog,
        hooks: Vec<ResolvedHook>,
        hook_semaphore: Arc<Semaphore>,
        hook_max_output_bytes: usize,
        archive_mailbox: impl Into<String>,
    ) -> Self {
        Self {
            db,
            mail,
            tags,
            drafts,
            events,
            hooks,
            hook_semaphore,
            hook_max_output_bytes,
            archive_mailbox: archive_mailbox.into(),
        }
    }

    /// The mailbox a `move_to`/`archive` resolves to, if any.
    fn destination<'a>(&'a self, actions: &'a Actions) -> Option<&'a str> {
        actions
            .move_to
            .as_deref()
            .or_else(|| actions.archive.then_some(self.archive_mailbox.as_str()))
    }

    /// Describe what `actions` would do to `facts`, without doing any of it.
    ///
    /// # Errors
    /// A mapped storage error from the existence checks. Nothing here
    /// mutates, so a failure leaves no partial state.
    pub async fn describe(
        &self,
        actions: &Actions,
        facts: &MessageFacts,
    ) -> Result<Vec<ActionOutcome>, Error> {
        let mut out = Vec::new();
        if let Some(dest) = self.destination(actions) {
            let key = if actions.archive {
                "archive"
            } else {
                "move_to"
            };
            match repo::mailbox_id(&self.db, facts.account_id, dest).await? {
                Some(_) => out.push(ActionOutcome::planned(
                    key,
                    format!("would move to {dest:?}"),
                )),
                None => out.push(ActionOutcome::planned(
                    key,
                    format!("would fail: this account has no mailbox named {dest:?}"),
                )),
            }
        }
        if !actions.add_labels.is_empty() {
            out.push(ActionOutcome::planned(
                "add_labels",
                format!("would apply {:?}", actions.add_labels),
            ));
        }
        if !actions.add_flags.is_empty() {
            let missing: Vec<&String> = actions
                .add_flags
                .iter()
                .filter(|f| !facts.flags.contains(*f))
                .collect();
            out.push(ActionOutcome::planned(
                "add_flags",
                if missing.is_empty() {
                    "would add nothing: every flag is already set".to_owned()
                } else {
                    format!("would add {missing:?}")
                },
            ));
        }
        if actions.notify {
            out.push(ActionOutcome::planned(
                "notify",
                "would publish a RULE_FIRED event",
            ));
        }
        if let Some(name) = &actions.run_hook {
            out.push(match self.hook(name) {
                Some(hook) => ActionOutcome::planned(
                    "run_hook",
                    format!("would run hook {:?} ({})", hook.name, hook.command),
                ),
                None => ActionOutcome::planned(
                    "run_hook",
                    format!("would fail: no enabled hook named {name:?} is configured"),
                ),
            });
        }
        if actions.draft_reply.is_some() {
            out.push(match self.reply_identity(facts).await? {
                Ok((from, to)) => ActionOutcome::planned(
                    "draft_reply",
                    format!("would draft a reply from {from} to {to}"),
                ),
                Err(why) => ActionOutcome::planned("draft_reply", format!("would fail: {why}")),
            });
        }
        Ok(out)
    }

    /// Fire `actions` against `facts`.
    ///
    /// Never returns `Err` — see the module docs.
    #[tracing::instrument(
        skip(self, actions, facts, cancel),
        fields(rule = rule_name, message_id = facts.message_id)
    )]
    pub async fn apply(
        &self,
        rule_name: &str,
        actions: &Actions,
        facts: &MessageFacts,
        cancel: &CancellationToken,
    ) -> Vec<ActionOutcome> {
        let mut out = Vec::new();

        // Labels and flags first, destination last: moving a message can
        // change its mailbox (and, on a real IMAP server, its UID), and the
        // tag/flag round trips address it by the mailbox/uid this snapshot
        // recorded. Doing the move first would leave the later actions
        // addressing a message that is no longer there.
        if !actions.add_labels.is_empty() {
            out.push(self.apply_labels(actions, facts).await);
        }
        if !actions.add_flags.is_empty() {
            out.push(self.apply_flags(actions, facts).await);
        }
        if let Some(name) = &actions.run_hook {
            out.push(self.apply_hook(name, rule_name, facts, cancel).await);
        }
        if let Some(body) = &actions.draft_reply {
            out.push(self.apply_draft_reply(body, facts).await);
        }
        if let Some(dest) = self.destination(actions) {
            let key = if actions.archive {
                "archive"
            } else {
                "move_to"
            };
            out.push(self.apply_move(key, dest, facts).await);
        }
        // Notify last, so the event announces a rule that has finished
        // acting rather than one still in progress — a `RULE_FIRED`
        // subscriber (including the hook dispatcher's own `on_rule_match`)
        // that immediately re-reads the message should see the result.
        if actions.notify {
            out.push(self.apply_notify(rule_name, facts, &out).await);
        }
        out
    }

    /// The *enabled* hook named `name`.
    ///
    /// The `enabled` filter is load-bearing, not tidiness.
    /// [`crate::hooks::resolve`] deliberately returns disabled hooks — a
    /// listing must show a hook an operator turned off — and documents that
    /// consumers which actually *fire* hooks have to filter themselves, as
    /// [`crate::hooks::HookDispatcher::new`] does. Without this, `enabled =
    /// false` (the operator's only kill switch for a hook) would be honoured
    /// by the event dispatcher and ignored by the rules engine, which is the
    /// more dangerous consumer of the two: unattended, recurring, and
    /// triggered by a *user*-authored rule rather than by an operator's
    /// explicit `TestHook`.
    fn hook(&self, name: &str) -> Option<&ResolvedHook> {
        self.hooks.iter().find(|h| h.name == name && h.enabled)
    }

    async fn apply_labels(&self, actions: &Actions, facts: &MessageFacts) -> ActionOutcome {
        match self
            .tags
            .add_tag(
                Target::Message(facts.message_id),
                &actions.add_labels,
                TagSource::Rule,
            )
            .await
        {
            Ok(applied) => ActionOutcome::ok(
                "add_labels",
                format!(
                    "applied {} of {} tag(s)",
                    applied.len(),
                    actions.add_labels.len()
                ),
            ),
            Err(error) => ActionOutcome::failed("add_labels", error.to_string()),
        }
    }

    async fn apply_flags(&self, actions: &Actions, facts: &MessageFacts) -> ActionOutcome {
        // `MailStore::set_flags` replaces the whole set (it is a `STORE
        // FLAGS`, not `+FLAGS`), so the union is computed here. A rule that
        // adds `\Seen` must not silently strip `\Flagged`.
        let mut union: BTreeSet<String> = facts.flags.clone();
        for flag in &actions.add_flags {
            union.insert(flag.clone());
        }
        if union.len() == facts.flags.len() {
            return ActionOutcome::ok("add_flags", "every flag was already set");
        }
        let flags: Vec<String> = union.into_iter().collect();
        match self.mail.set_flags(facts.message_id, flags).await {
            Ok(_) => ActionOutcome::ok("add_flags", format!("added {:?}", actions.add_flags)),
            Err(error) => ActionOutcome::failed("add_flags", error.to_string()),
        }
    }

    async fn apply_move(&self, key: &str, dest: &str, facts: &MessageFacts) -> ActionOutcome {
        let dest_id = match repo::mailbox_id(&self.db, facts.account_id, dest).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return ActionOutcome::failed(
                    key,
                    format!("this account has no mailbox named {dest:?}"),
                )
            }
            Err(error) => return ActionOutcome::failed(key, error.to_string()),
        };
        if dest_id == facts.mailbox_id {
            return ActionOutcome::ok(key, format!("already in {dest:?}"));
        }
        match self.mail.move_message(facts.message_id, dest_id).await {
            Ok(()) => ActionOutcome::ok(key, format!("moved to {dest:?}")),
            Err(error) => ActionOutcome::failed(key, error.to_string()),
        }
    }

    async fn apply_hook(
        &self,
        name: &str,
        rule_name: &str,
        facts: &MessageFacts,
        cancel: &CancellationToken,
    ) -> ActionOutcome {
        let Some(hook) = self.hook(name) else {
            return ActionOutcome::failed(
                "run_hook",
                format!("no enabled hook named {name:?} is configured"),
            );
        };
        // Bounded by the same budget the hook dispatcher draws from, so rule-
        // fired hooks and event-fired hooks cannot together exceed
        // `hooks.max_concurrency`. Raced against `cancel` rather than awaited
        // unconditionally: this sits inside the claim-then-act window, so a
        // shutdown that lands here would otherwise wait for a permit that a
        // shutting-down daemon may never release, holding a claim it can no
        // longer honour.
        let permit = tokio::select! {
            () = cancel.cancelled() => {
                return ActionOutcome::failed(
                    "run_hook",
                    "cancelled while waiting for hook concurrency capacity",
                );
            }
            permit = Arc::clone(&self.hook_semaphore).acquire_owned() => permit,
        };
        let Ok(_permit) = permit else {
            return ActionOutcome::failed("run_hook", "the hook concurrency budget is unavailable");
        };
        // The payload reaches the hook on stdin and nowhere else — never
        // interpolated into argv. See `crate::hooks`'s module docs; that
        // invariant is what makes it safe for this JSON to carry an
        // attacker-controlled subject line.
        let payload = serde_json::to_vec(&serde_json::json!({
            "seq": 0,
            "kind": EventKind::RuleFired.as_str(),
            "account_id": facts.account_id,
            "mailbox_id": facts.mailbox_id,
            "message_id": facts.message_id,
            "at": chrono::Utc::now().timestamp(),
            "payload": rule_payload(rule_name, facts),
        }))
        .unwrap_or_default();
        let outcome = hooks::run_hook(
            &hook.command,
            &hook.args,
            hook.timeout,
            self.hook_max_output_bytes,
            &payload,
            cancel,
        )
        .await;
        if outcome.succeeded() {
            ActionOutcome::ok("run_hook", format!("hook {name:?} exited 0"))
        } else {
            ActionOutcome::failed(
                "run_hook",
                format!(
                    "hook {name:?} failed (timed_out={}, cancelled={}, exit_code={:?})",
                    outcome.timed_out, outcome.cancelled, outcome.exit_code
                ),
            )
        }
    }

    /// The `(from, to)` a reply draft would use, or why one cannot be built.
    ///
    /// The outer `Result` is a storage failure; the inner one is "this
    /// message cannot be replied to," which is a reportable action outcome
    /// rather than an error.
    async fn reply_identity(
        &self,
        facts: &MessageFacts,
    ) -> Result<Result<(String, String), String>, Error> {
        let Some(to_addr) = facts.from_addr.as_deref().filter(|a| !a.is_empty()) else {
            return Ok(Err(
                "the message has no sender address to reply to".to_owned()
            ));
        };
        let Some(username) = repo::account_username(&self.db, facts.account_id).await? else {
            return Ok(Err(
                "this account has no configured username to send as".to_owned()
            ));
        };
        Ok(Ok((username, to_addr.to_owned())))
    }

    async fn apply_draft_reply(&self, body: &str, facts: &MessageFacts) -> ActionOutcome {
        let (from, to) = match self.reply_identity(facts).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(why)) => return ActionOutcome::failed("draft_reply", why),
            Err(error) => return ActionOutcome::failed("draft_reply", error.to_string()),
        };
        let from = match Mailbox::new(&from, None) {
            Ok(mailbox) => mailbox,
            Err(error) => {
                return ActionOutcome::failed(
                    "draft_reply",
                    format!("the account username is not a usable address: {error}"),
                )
            }
        };
        let to = match Mailbox::new(&to, facts.from_name.as_deref()) {
            Ok(mailbox) => mailbox,
            Err(error) => {
                return ActionOutcome::failed(
                    "draft_reply",
                    format!("the sender address is not usable: {error}"),
                )
            }
        };
        let mut body = body.to_owned();
        if let Some((idx, _)) = body.char_indices().nth(MAX_DRAFT_BODY) {
            body.truncate(idx);
        }
        let draft = NewDraft {
            account_id: facts.account_id,
            from,
            to: vec![to],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: reply_subject(&facts.subject),
            body_text: body,
            body_html: None,
            attachments: Vec::new(),
            // Threading headers are frozen onto the draft from this message —
            // a reply that does not thread is a reply the recipient reads out
            // of context.
            in_reply_to_message_id: Some(facts.message_id),
        };
        match self.drafts.create(draft).await {
            Ok(draft) => ActionOutcome::ok("draft_reply", format!("created draft {}", draft.id)),
            Err(error) => ActionOutcome::failed("draft_reply", error.to_string()),
        }
    }

    async fn apply_notify(
        &self,
        rule_name: &str,
        facts: &MessageFacts,
        so_far: &[ActionOutcome],
    ) -> ActionOutcome {
        let mut payload = rule_payload(rule_name, facts);
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "actions".to_owned(),
                serde_json::Value::Array(
                    so_far
                        .iter()
                        .map(|o| {
                            serde_json::json!({
                                "action": o.action,
                                "applied": o.applied,
                                "detail": o.detail,
                            })
                        })
                        .collect(),
                ),
            );
        }
        let event = NewEvent::new(EventKind::RuleFired)
            .account(facts.account_id)
            .mailbox(facts.mailbox_id)
            .message(facts.message_id)
            .payload(payload);
        match self.events.append(event).await {
            Ok(event) => ActionOutcome::ok("notify", format!("published event {}", event.seq)),
            Err(error) => ActionOutcome::failed("notify", error.to_string()),
        }
    }
}

/// The `RULE_FIRED` payload. `rule` matches what
/// `crate::hooks::sample_event_json` documents for `on_rule_match`, so a hook
/// script written against the sample works against the real thing.
fn rule_payload(rule_name: &str, facts: &MessageFacts) -> serde_json::Value {
    serde_json::json!({
        "rule": rule_name,
        "mailbox": facts.mailbox,
        "subject": facts.subject,
        "from": facts.from,
        "rfc_message_id": facts.rfc_message_id,
    })
}

/// `Re: ` the subject, without stacking a second prefix on a subject that
/// already has one (case-insensitively, as every mail client does).
fn reply_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.is_empty() {
        return "Re:".to_owned();
    }
    if trimmed.len() >= 3 && trimmed[..3].eq_ignore_ascii_case("re:") {
        return trimmed.to_owned();
    }
    format!("Re: {trimmed}")
}
