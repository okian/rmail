//! The closed action vocabulary, and the parse that is the only way into it.
//!
//! # Why this is an enum and not a string
//!
//! The model reading a message decides what happens to it. If that decision
//! travelled as a string — a tool name, a mailbox, an IMAP command — then a
//! body that talks the model into writing a different string would have chosen
//! a different effect, and the blast radius of one successful injection would
//! be "whatever the executor's string match happens to accept". As an enum
//! with five inhabitants, the blast radius is instead "one of the five things
//! the operator already agreed the agent may do", which is a bound that holds
//! no matter what the model writes.
//!
//! So: [`ActionKind`] is a closed set, [`Decision::parse`] is total, and
//! anything outside the set is [`Refusal`] — never a default, never a
//! fallback, never "the closest match". A fallback would reintroduce exactly
//! the property the enum removes: `"delete_everything"` quietly becoming
//! `archive` is worse than a refusal, because it is a mutation nobody asked
//! for wearing a name somebody did.
//!
//! # Each action's parameter is itself closed
//!
//! An enum with a `String` payload the model fills freely is a closed
//! vocabulary in name only. Every variant here is either parameterless or
//! carries a value validated against something the operator wrote down:
//!
//! | action | parameter | who chooses it |
//! |---|---|---|
//! | `archive` | none | the destination is `agent.archive_mailbox`, config |
//! | `label` | a tag name | must be in `agent.labels`, config |
//! | `snooze` | hours | integer, `1..=agent.max_snooze_hours`; the tag is `agent.snooze_tag` |
//! | `draft_reply` | body text | the model — but a draft sends nothing |
//! | `escalate` | none | — |
//!
//! `draft_reply` is the one place model-authored free text survives, and it
//! survives into a *draft*: an inert document a human opens, reads and edits
//! before anything leaves the machine. It is still length-bounded and put
//! through [`crate::ai::injection::sanitize_model_text`], because that text
//! reaches a terminal.

use crate::ai::injection;
use crate::error::Error;

/// The five things an inbox agent may do, plus the sixth that is "nothing".
///
/// `Copy` and tiny on purpose: this crosses the boundary between the model's
/// answer and the mutation layer many times per run, and a type that could
/// carry a surprise is the wrong shape for that boundary. The *parameters*
/// live on [`Decision`], not here, so a match on the kind alone is exhaustive
/// and stays exhaustive when a parameter changes shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActionKind {
    /// Move the message to the operator-configured archive mailbox.
    Archive,
    /// Apply one operator-configured label.
    Label,
    /// Defer the message until a bounded time in the future: the agent stops
    /// reconsidering it until then, and it is tagged so a human can see that.
    /// Does not remove it from any listing — see [`super::apply`].
    Snooze,
    /// Stage an editable reply draft. Sends nothing — see [`super::apply`].
    DraftReply,
    /// Flag the message and announce it, so a human looks.
    Escalate,
    /// Leave the message exactly as it is.
    None,
}

impl ActionKind {
    /// Every kind, for exhaustive handling and tests.
    pub const ALL: [Self; 6] = [
        Self::Archive,
        Self::Label,
        Self::Snooze,
        Self::DraftReply,
        Self::Escalate,
        Self::None,
    ];

    /// The kinds the *model* may choose. Excludes nothing today — `none` is a
    /// legitimate answer and the one the prompt asks for when in doubt — but
    /// is the list the schema enumerates, kept separate from [`Self::ALL`] so
    /// adding an internal-only kind later does not silently offer it to the
    /// model.
    pub const SELECTABLE: [Self; 6] = Self::ALL;

    /// The wire/storage string. This is `agent_actions.action`'s CHECK list
    /// and the JSON Schema's `enum`, so the three cannot drift.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Label => "label",
            Self::Snooze => "snooze",
            Self::DraftReply => "draft_reply",
            Self::Escalate => "escalate",
            Self::None => "none",
        }
    }

    /// Parse a wire string, or `None`.
    ///
    /// Deliberately exact: no trimming beyond the caller's own, no
    /// case-folding, no prefix match. Every loosening is a way for a model
    /// that was steered into writing `"Archive all"` to be understood as
    /// `archive`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }

    /// Whether performing this changes anything an observer outside this
    /// process could see.
    ///
    /// [`Self::Snooze`] counts: it writes a durable row and applies a tag, both
    /// of which outlive the run. [`Self::None`] does not, which is what makes a
    /// run of nothing but `none` decisions free of the mutate allowlist.
    #[must_use]
    pub const fn mutates(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Why a model's answer was not turned into an action.
///
/// Carried rather than collapsed into a bare error because it is *logged*: a
/// user looking at a run that did nothing needs to see "the model asked to
/// delete this" rather than an empty list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// One sentence naming what was wrong, safe to print.
    pub detail: String,
}

impl Refusal {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

/// The longest reason retained. The prompt asks for one sentence; this is the
/// enforcement point, since the schema subset `output_format` accepts cannot
/// express `maxLength`.
pub const MAX_REASON_CHARS: usize = 400;

/// The longest `draft_reply` body accepted. A reply this agent writes
/// unattended is a first draft for a human to edit, not a document; past this
/// it is the model running away with the turn.
pub const MAX_DRAFT_BODY_CHARS: usize = 4_000;

/// One validated decision about one message: what to do, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// What to do.
    pub kind: ActionKind,
    /// The label to apply, for [`ActionKind::Label`]. Always one of the
    /// operator's configured labels — [`Decision::parse`] rejects anything
    /// else — and empty for every other kind.
    pub label: String,
    /// Hours to hide the message for, for [`ActionKind::Snooze`]. Always
    /// inside the configured bound; zero for every other kind.
    pub snooze_hours: u32,
    /// The reply body, for [`ActionKind::DraftReply`]. Sanitized and
    /// truncated; empty for every other kind.
    pub body: String,
    /// The model's stated reason, sanitized and truncated. Never empty — an
    /// answer with no reason is refused, because prd.md #47's whole ask is a
    /// log that says *why*.
    pub reason: String,
}

/// What the operator has agreed this agent may choose from.
///
/// Passed to [`Decision::parse`] rather than consulted globally, so the
/// validation and the thing it validates against arrive at the same place and
/// a test can vary one without the other.
#[derive(Debug, Clone)]
pub struct Vocabulary<'a> {
    /// The labels `label` may name. An empty list makes `label` unavailable —
    /// the parse refuses it — rather than admitting any string.
    pub labels: &'a [String],
    /// The largest snooze, in hours.
    pub max_snooze_hours: u32,
}

impl Vocabulary<'_> {
    /// The `enum` list the JSON Schema constrains `action` to.
    ///
    /// `label` is dropped when no labels are configured: offering the model an
    /// action whose every argument would be refused wastes a call and reads,
    /// in the log, as the agent malfunctioning rather than as the operator not
    /// having configured it.
    #[must_use]
    pub fn selectable(&self) -> Vec<&'static str> {
        ActionKind::SELECTABLE
            .into_iter()
            .filter(|kind| !(*kind == ActionKind::Label && self.labels.is_empty()))
            .map(ActionKind::as_str)
            .collect()
    }
}

/// The model's raw answer, exactly as the schema shapes it.
///
/// Every field is defaulted so a model that omits one produces a [`Refusal`]
/// with a sentence naming the omission, rather than a deserialization error
/// naming a Rust field.
///
/// Deliberately *not* `deny_unknown_fields`, matching
/// [`crate::rules::classify`]'s own answer type. A model that helpfully adds a
/// `"confidence"` the schema did not ask for is a model quirk, and aborting the
/// whole run over it would turn "the agent stopped triaging" into something an
/// operator has to debug. Ignoring it costs nothing, because only the fields
/// below are ever read and every one of them is validated afterwards — an
/// extra key cannot smuggle anything past a closed vocabulary.
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct RawAnswer {
    #[serde(default)]
    action: String,
    #[serde(default)]
    label: String,
    /// `i64`, not `u32`, on purpose. A model that answers `-1` or `1e9` must
    /// produce a *refusal* — an entry in the log saying what it asked for —
    /// not a serde error that ends the whole run. Range-checking here rather
    /// than in the type is what keeps "an answer outside the vocabulary is
    /// refused, never fatal" true for the parameters as well as for the verb.
    #[serde(default)]
    snooze_hours: i64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    reason: String,
}

impl Decision {
    /// A parameterless decision, for the kinds that take none.
    fn bare(kind: ActionKind, reason: String) -> Self {
        Self {
            kind,
            label: String::new(),
            snooze_hours: 0,
            body: String::new(),
            reason,
        }
    }

    /// Turn one model response into a decision, or refuse it.
    ///
    /// The outer `Result` is "this is not even JSON of the requested shape",
    /// which is a provider/schema failure the caller reports as an error. The
    /// inner `Result` is "the model answered, and the answer is not something
    /// this agent may do", which is a *logged refusal* — the run continues,
    /// the message is left alone, and the log says what was asked for.
    ///
    /// # Errors
    /// [`Error::Internal`] when `text` is not JSON matching the schema.
    pub fn parse(text: &str, vocabulary: &Vocabulary<'_>) -> Result<Result<Self, Refusal>, Error> {
        let raw: RawAnswer = serde_json::from_str(text).map_err(|e| {
            Error::internal(format!(
                "an inbox-agent decision did not match the requested schema: {e}"
            ))
        })?;

        // The reason is sanitized up front, so a refusal path and an accept
        // path put the same characters in the log.
        let reason = clamp(
            &injection::sanitize_model_text(raw.reason.trim()),
            MAX_REASON_CHARS,
        );

        // The closed set, checked *before* the empty-reason check. Ordering
        // matters for what the log ends up saying: a model steered into
        // `"delete_everything"` with no reason attached is far more
        // interesting as "it asked to delete_everything" than as "it gave no
        // reason", and refusing on the reason first would throw the verb away
        // — the one detail this log exists to capture.
        //
        // An unrecognised verb is quoted back (sanitized — it is
        // model-authored text on its way to a terminal), never mapped to a
        // neighbour.
        let Some(kind) = ActionKind::parse(raw.action.trim()) else {
            let asked = clamp(
                &injection::sanitize_model_text(raw.action.trim()),
                MAX_ACTION_ECHO_CHARS,
            );
            return Ok(Err(Refusal::new(format!(
                "the model asked for action {asked:?}, which is not one of the actions this \
                 agent may take ({}); refused",
                vocabulary.selectable().join(", ")
            ))));
        };

        if reason.is_empty() {
            return Ok(Err(Refusal::new(format!(
                "the model asked for action {:?} and gave no reason; an unattended action with \
                 no stated reason is not auditable, so it was refused",
                kind.as_str()
            ))));
        }

        match kind {
            ActionKind::Archive | ActionKind::Escalate | ActionKind::None => {
                Ok(Ok(Self::bare(kind, reason)))
            }
            ActionKind::Label => {
                let asked = raw.label.trim();
                // Matched against the operator's list by exact string. The
                // model does not get to name a tag: it gets to pick one of
                // the tags a human already agreed to, and `get_or_create_tag`
                // downstream would otherwise happily mint whatever it wrote.
                let Some(label) = vocabulary.labels.iter().find(|l| l.as_str() == asked) else {
                    let asked = clamp(
                        &injection::sanitize_model_text(asked),
                        MAX_ACTION_ECHO_CHARS,
                    );
                    return Ok(Err(Refusal::new(format!(
                        "the model asked to apply label {asked:?}, which is not one of the \
                         labels this agent may apply ({}); refused",
                        if vocabulary.labels.is_empty() {
                            "none are configured".to_owned()
                        } else {
                            vocabulary.labels.join(", ")
                        }
                    ))));
                };
                Ok(Ok(Self {
                    kind,
                    label: label.clone(),
                    snooze_hours: 0,
                    body: String::new(),
                    reason,
                }))
            }
            ActionKind::Snooze => {
                // Refused rather than clamped. A clamp turns "hide this for a
                // year" into "hide this for a week" and logs the week, which
                // hides that the model asked for something the operator had
                // ruled out; the refusal is the honest record.
                let hours = u32::try_from(raw.snooze_hours).unwrap_or(0);
                if hours == 0 || hours > vocabulary.max_snooze_hours {
                    return Ok(Err(Refusal::new(format!(
                        "the model asked to snooze for {} hour(s), outside the permitted \
                         1..={} hours; refused",
                        raw.snooze_hours, vocabulary.max_snooze_hours
                    ))));
                }
                Ok(Ok(Self {
                    kind,
                    label: String::new(),
                    snooze_hours: hours,
                    body: String::new(),
                    reason,
                }))
            }
            ActionKind::DraftReply => {
                let body = clamp(
                    &injection::sanitize_model_text(raw.body.trim()),
                    MAX_DRAFT_BODY_CHARS,
                );
                if body.is_empty() {
                    return Ok(Err(Refusal::new(
                        "the model asked to draft a reply but wrote no body; refused rather \
                         than staging an empty draft",
                    )));
                }
                Ok(Ok(Self {
                    kind,
                    label: String::new(),
                    snooze_hours: 0,
                    body,
                    reason,
                }))
            }
        }
    }
}

/// How much of a rejected verb is echoed back into the log. Short: this is
/// attacker-influenced text, and the log entry's job is to identify what was
/// asked for, not to reproduce it.
const MAX_ACTION_ECHO_CHARS: usize = 64;

/// Truncate on a character boundary.
fn clamp(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => text.get(..idx).unwrap_or_default().to_owned(),
        None => text.to_owned(),
    }
}
