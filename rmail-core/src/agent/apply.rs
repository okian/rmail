//! Performing a decided action — and describing what one *would* do.
//!
//! # Nothing here sends mail, and nothing here deletes anything
//!
//! That is a structural property, not a runtime check.
//!
//! `draft_reply` terminates at [`crate::compose::DraftStore::create`], exactly
//! as task 62's `crate::compose::reply` does, and this module names no outbox,
//! SMTP or submission symbol at all —
//! `agent::tests::nothing_in_the_agent_can_reach_the_send_path` reads the
//! source back and fails if one appears. Likewise for deletion: the mutations
//! reachable from here are a MOVE, a keyword STORE, a local tag row and a local
//! snooze row. `MailStore::delete_message` is never named, and neither is
//! `EXPUNGE`.
//!
//! A test asserting "the outbox was empty afterwards" would pass for every
//! reason except the one that matters, including on a build where this module
//! had grown an `OutboxStore` that simply had not been reached yet. The
//! difference is between "did not send this time" and "cannot send", and for a
//! loop driven by a model reading attacker-authored text only the second is
//! worth anything.
//!
//! # Nothing here fails a run
//!
//! [`Executor::apply`] never returns `Err`. Every action produces an
//! [`AppliedOutcome`] saying whether it landed and why not, and the loop moves
//! on — [`crate::rules::actions`]' contract, one level up, for the same
//! reason: an archive naming a mailbox the account does not have is an
//! ordinary misconfiguration, and letting it abort the run turns one typo into
//! an inbox nobody triaged.
//!
//! # A dry run touches nothing
//!
//! [`Executor::describe`] performs the same existence lookups `apply` does, so
//! a dry run reports the misconfiguration a real run would hit, and issues no
//! mutation, no IMAP round trip, no draft and no row.

use crate::compose::{DraftStore, Mailbox, NewDraft};
use crate::error::Error;
use crate::events::{EventKind, EventLog, NewEvent};
use crate::mail::MailStore;
use crate::rules::facts::MessageFacts;
use crate::rules::repo;
use crate::storage::Database;
use crate::tags::{TagSource, TagStore, Target};

use super::action::{ActionKind, Decision};

/// What one action did, or would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedOutcome {
    /// Whether the mutation landed. Always `false` on the describe path.
    pub applied: bool,
    /// Human-readable detail: what it did, or why it could not.
    pub detail: String,
}

impl AppliedOutcome {
    fn ok(detail: impl Into<String>) -> Self {
        Self {
            applied: true,
            detail: detail.into(),
        }
    }

    fn failed(detail: impl Into<String>) -> Self {
        Self {
            applied: false,
            detail: detail.into(),
        }
    }
}

/// Everything a decided action needs to reach.
///
/// Cheap to clone — every field is a handle or a small owned value.
#[derive(Debug, Clone)]
pub struct Executor {
    db: Database,
    mail: MailStore,
    tags: TagStore,
    drafts: DraftStore,
    events: EventLog,
    /// Where `archive` moves to. Operator configuration, never the model's
    /// choice — a mailbox name the model could pick would be a MOVE to
    /// anywhere in the account.
    archive_mailbox: String,
    /// The tag `snooze` applies. Operator configuration for the same reason
    /// the label allowlist is: `get_or_create_tag` mints whatever name it is
    /// handed, so a name the model could choose would be a model-authored
    /// write into the user's tag namespace.
    snooze_tag: String,
}

/// The IMAP flag `escalate` sets. `\Flagged` is the one "needs attention"
/// marker every mail client already renders, so an escalation is visible
/// wherever the user reads their mail rather than only in this daemon's log.
pub const ESCALATE_FLAG: &str = "\\Flagged";

impl Executor {
    /// Build an executor.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        mail: MailStore,
        tags: TagStore,
        drafts: DraftStore,
        events: EventLog,
        archive_mailbox: impl Into<String>,
        snooze_tag: impl Into<String>,
    ) -> Self {
        Self {
            db,
            mail,
            tags,
            drafts,
            events,
            archive_mailbox: archive_mailbox.into(),
            snooze_tag: snooze_tag.into(),
        }
    }

    /// The archive destination, for the log and for error messages.
    #[must_use]
    pub fn archive_mailbox(&self) -> &str {
        &self.archive_mailbox
    }

    /// The tag a `snooze` applies.
    #[must_use]
    pub fn snooze_tag(&self) -> &str {
        &self.snooze_tag
    }

    /// The action's validated parameter, rendered for the log.
    ///
    /// Never the model's raw text: a `draft_reply` body is summarized by its
    /// length rather than copied, because the log is read in a terminal and
    /// the body is the one field the model still writes freely.
    #[must_use]
    pub fn argument(&self, decision: &Decision) -> String {
        match decision.kind {
            ActionKind::Archive => self.archive_mailbox.clone(),
            ActionKind::Label => decision.label.clone(),
            ActionKind::Snooze => format!("{}h", decision.snooze_hours),
            ActionKind::DraftReply => format!("{} chars", decision.body.chars().count()),
            ActionKind::Escalate | ActionKind::None => String::new(),
        }
    }

    /// Describe what `decision` would do to `facts`, without doing any of it.
    ///
    /// # Errors
    /// A mapped storage error from the existence checks. Nothing here mutates,
    /// so a failure leaves no partial state.
    pub async fn describe(
        &self,
        decision: &Decision,
        facts: &MessageFacts,
    ) -> Result<AppliedOutcome, Error> {
        Ok(match decision.kind {
            ActionKind::None => AppliedOutcome::failed("would leave this message alone"),
            ActionKind::Archive => {
                let dest = &self.archive_mailbox;
                match repo::mailbox_id(&self.db, facts.account_id, dest).await? {
                    Some(id) if id == facts.mailbox_id => {
                        AppliedOutcome::failed(format!("already in {dest:?}"))
                    }
                    Some(_) => AppliedOutcome::failed(format!("would move to {dest:?}")),
                    None => AppliedOutcome::failed(format!(
                        "would fail: this account has no mailbox named {dest:?}"
                    )),
                }
            }
            ActionKind::Label => {
                AppliedOutcome::failed(format!("would apply label {:?}", decision.label))
            }
            ActionKind::Snooze => AppliedOutcome::failed(format!(
                "would defer this message for {} hour(s) and tag it {:?}",
                decision.snooze_hours, self.snooze_tag
            )),
            ActionKind::Escalate => {
                AppliedOutcome::failed(if facts.flags.contains(ESCALATE_FLAG) {
                    "would announce this message; it is already flagged".to_owned()
                } else {
                    format!("would flag {ESCALATE_FLAG} and announce this message")
                })
            }
            ActionKind::DraftReply => match self.reply_identity(facts).await? {
                Ok((from, to)) => {
                    AppliedOutcome::failed(format!("would draft a reply from {from} to {to}"))
                }
                Err(why) => AppliedOutcome::failed(format!("would fail: {why}")),
            },
        })
    }

    /// Perform `decision` against `facts`.
    ///
    /// Never returns `Err` — see the module docs.
    #[tracing::instrument(
        skip(self, decision, facts),
        fields(action = decision.kind.as_str(), message_id = facts.message_id)
    )]
    pub async fn apply(&self, decision: &Decision, facts: &MessageFacts) -> AppliedOutcome {
        match decision.kind {
            // Reachable: `none` is a real decision the model makes, and it is
            // logged like any other so the run reports what it considered.
            ActionKind::None => AppliedOutcome::ok("left alone"),
            ActionKind::Archive => self.archive(facts).await,
            ActionKind::Label => self.label(decision, facts).await,
            ActionKind::Snooze => self.snooze(decision, facts).await,
            ActionKind::Escalate => self.escalate(decision, facts).await,
            ActionKind::DraftReply => self.draft_reply(decision, facts).await,
        }
    }

    async fn archive(&self, facts: &MessageFacts) -> AppliedOutcome {
        let dest = &self.archive_mailbox;
        let dest_id = match repo::mailbox_id(&self.db, facts.account_id, dest).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return AppliedOutcome::failed(format!(
                    "this account has no mailbox named {dest:?}"
                ))
            }
            Err(error) => return AppliedOutcome::failed(error.to_string()),
        };
        if dest_id == facts.mailbox_id {
            return AppliedOutcome::ok(format!("already in {dest:?}"));
        }
        match self.mail.move_message(facts.message_id, dest_id).await {
            Ok(()) => AppliedOutcome::ok(format!("moved to {dest:?}")),
            Err(error) => AppliedOutcome::failed(error.to_string()),
        }
    }

    async fn label(&self, decision: &Decision, facts: &MessageFacts) -> AppliedOutcome {
        // `TagSource::Rule`, matching `crate::rules::actions`: an unattended
        // automation applying a label a human configured. Deliberately not
        // `TagSource::Ai`, which that enum reserves for the *suggestion*
        // pipeline where a tag stays `Pending` until a human resolves it —
        // this tag is applied, and mislabelling its provenance would make
        // `mail tags` show a pending suggestion that no longer exists.
        let names = [decision.label.clone()];
        match self
            .tags
            .add_tag(Target::Message(facts.message_id), &names, TagSource::Rule)
            .await
        {
            Ok(applied) if applied.is_empty() => {
                AppliedOutcome::ok(format!("label {:?} was already applied", decision.label))
            }
            Ok(_) => AppliedOutcome::ok(format!("applied label {:?}", decision.label)),
            Err(error) => AppliedOutcome::failed(error.to_string()),
        }
    }

    /// Defer this message until `until`, and mark it so a human can see that
    /// it was deferred.
    ///
    /// # What a snooze does, precisely
    ///
    /// It writes `message_snoozes` — which [`super::store::candidates`] reads,
    /// so the agent itself stops reconsidering the message until the time
    /// passes and then picks it up again — and applies
    /// [`Executor::snooze_tag`], which puts it in the tag namespace every
    /// existing surface already reads (`mail tags`, `tag:` in search, smart
    /// folders).
    ///
    /// It does **not** remove the message from the user's inbox listing, and
    /// the docs deliberately no longer say it does. `MailStore::list` joins no
    /// snooze table, and making it do so would change the meaning of every
    /// listing in the product on behalf of one model-chosen action — which is
    /// a much larger decision than this feature gets to make. A snooze the
    /// user cannot see would be worse than one that is only a marker: this way
    /// the state is visible, filterable and removable with `mail untag`.
    async fn snooze(&self, decision: &Decision, facts: &MessageFacts) -> AppliedOutcome {
        let hours = i64::from(decision.snooze_hours);
        let until = chrono::Utc::now().timestamp().saturating_add(hours * 3_600);
        if let Err(error) =
            super::store::snooze(&self.db, facts.message_id, until, &decision.reason).await
        {
            return AppliedOutcome::failed(error.to_string());
        }
        // The tag is operator configuration, never the model's choice — the
        // same rule the `label` action's allowlist enforces, for the same
        // reason: an unknown tag name is simply created.
        let names = [self.snooze_tag.clone()];
        match self
            .tags
            .add_tag(Target::Message(facts.message_id), &names, TagSource::Rule)
            .await
        {
            Ok(_) => AppliedOutcome::ok(format!(
                "deferred for {hours} hour(s) and tagged {:?}",
                self.snooze_tag
            )),
            // The deferral landed and the marker did not. Still `applied`: the
            // mutation happened and must count against the blast-radius bound,
            // and the detail says which half is missing rather than claiming
            // both worked.
            Err(error) => AppliedOutcome::ok(format!(
                "deferred for {hours} hour(s), but could not tag it {:?}: {error}",
                self.snooze_tag
            )),
        }
    }

    async fn escalate(&self, decision: &Decision, facts: &MessageFacts) -> AppliedOutcome {
        // `MailStore::set_flags` replaces the whole set (it is a `STORE
        // FLAGS`, not `+FLAGS`), so the union is computed here: escalating
        // must not strip `\Seen` or anything else the message already
        // carries.
        let detail = if facts.flags.contains(ESCALATE_FLAG) {
            "already flagged".to_owned()
        } else {
            let mut union: Vec<String> = facts.flags.iter().cloned().collect();
            union.push(ESCALATE_FLAG.to_owned());
            match self.mail.set_flags(facts.message_id, union).await {
                Ok(_) => format!("flagged {ESCALATE_FLAG}"),
                Err(error) => return AppliedOutcome::failed(error.to_string()),
            }
        };

        // Announced on the durable event log rather than through
        // `crate::notify`: an escalation is "a human should look", which is
        // what hooks, webhooks and the alert stream all already subscribe to,
        // and wiring the notification engine in here would give this module a
        // second scoring pass over mail it has already had a model read.
        //
        // `RuleFired` rather than a new `EventKind`, on the precedent
        // `crate::smart_folder` sets for exactly this situation: the kind is
        // "an automation acted on this message", the payload says which
        // automation, and a new variant would silently not reach subscribers
        // written against the existing set.
        let event = NewEvent::new(EventKind::RuleFired)
            .account(facts.account_id)
            .mailbox(facts.mailbox_id)
            .message(facts.message_id)
            .payload(serde_json::json!({
                "rule": "inbox-agent",
                "action": ActionKind::Escalate.as_str(),
                "reason": decision.reason,
                "mailbox": facts.mailbox,
                "subject": facts.subject,
                "from": facts.from,
                "rfc_message_id": facts.rfc_message_id,
            }));
        match self.events.append(event).await {
            Ok(event) => AppliedOutcome::ok(format!("{detail}; announced as event {}", event.seq)),
            // The flag landed and the announcement did not.
            //
            // Reported as `applied` with both halves named, not as a failure.
            // The flag is the half a human sees — it is what every mail client
            // renders — and the IMAP mutation has already happened, so calling
            // this "failed" would both misdescribe it and, because
            // `actions_applied` only counts applied actions, let it escape the
            // `max_actions` blast-radius bound that is supposed to cap how
            // much mail one run can touch.
            Err(error) => AppliedOutcome::ok(format!(
                "{detail}, but the escalation could not be announced: {error}"
            )),
        }
    }

    /// The `(from, to)` a reply draft would use, or why one cannot be built.
    ///
    /// The outer `Result` is a storage failure; the inner one is "this message
    /// cannot be replied to", which is a reportable outcome rather than an
    /// error.
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
                "this account has no configured username to draft as".to_owned()
            ));
        };
        Ok(Ok((username, to_addr.to_owned())))
    }

    async fn draft_reply(&self, decision: &Decision, facts: &MessageFacts) -> AppliedOutcome {
        let (from, to) = match self.reply_identity(facts).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(why)) => return AppliedOutcome::failed(why),
            Err(error) => return AppliedOutcome::failed(error.to_string()),
        };
        let from = match Mailbox::new(&from, None) {
            Ok(mailbox) => mailbox,
            Err(error) => {
                return AppliedOutcome::failed(format!(
                    "the account username is not a usable address: {error}"
                ))
            }
        };
        let to = match Mailbox::new(&to, facts.from_name.as_deref()) {
            Ok(mailbox) => mailbox,
            Err(error) => {
                return AppliedOutcome::failed(format!("the sender address is not usable: {error}"))
            }
        };
        let draft = NewDraft {
            account_id: facts.account_id,
            from,
            to: vec![to],
            // Never cc or bcc. The model chose to reply to one sender; a
            // widened recipient list is a disclosure decision, and this agent
            // does not get to make one.
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: reply_subject(&facts.subject),
            body_text: decision.body.clone(),
            body_html: None,
            attachments: Vec::new(),
            in_reply_to_message_id: Some(facts.message_id),
        };
        match self.drafts.create(draft).await {
            Ok(draft) => {
                AppliedOutcome::ok(format!("created draft {} (nothing was sent)", draft.id))
            }
            Err(error) => AppliedOutcome::failed(error.to_string()),
        }
    }
}

/// `Re: ` the subject, without stacking a second prefix on one that already
/// has it (case-insensitively, as every mail client does).
fn reply_subject(subject: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.is_empty() {
        return "Re:".to_owned();
    }
    if trimmed.len() >= 3
        && trimmed
            .get(..3)
            .is_some_and(|p| p.eq_ignore_ascii_case("re:"))
    {
        return trimmed.to_owned();
    }
    format!("Re: {trimmed}")
}
