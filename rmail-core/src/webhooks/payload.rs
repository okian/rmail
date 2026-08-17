//! What a webhook actually says — the one file to read to know what leaves
//! this machine.
//!
//! # The default payload is a notification, not the mail
//!
//! Every delivery carries these fields and, by default, nothing else:
//!
//! | field | what it is | why it is the default |
//! |---|---|---|
//! | `event` | `on_new_message`, `on_rule_match`, `forward`, ... | the receiver has to route on something |
//! | `delivery_id` | this delivery's stable id | the dedupe key for an at-least-once retry (see V48) |
//! | `message.id` | the local row id | what `deep_link` and every rmail surface address a message by |
//! | `message.link` | `rmail://message/<id>` | "click here" — the whole point of a notification |
//! | `message.from` | the sender, **sanitized** | see below on why this one is not redacted |
//! | `message.subject` | the subject, **redacted** | the human-readable "what is this" |
//! | `message.date` | unix seconds | ordering, and "is this old" |
//! | `message.account` / `message.mailbox` | where it landed | an operator with three accounts needs to know which |
//! | `message.rfc_message_id` | the `Message-ID`, **sanitized** | correlating against a different mail store |
//!
//! Not included, at any setting: attachments, attachment filenames, recipient
//! lists, raw MIME, or any header other than the three above (`From:`,
//! `Subject:`, `Message-ID:`). Those are neither needed to decide whether to go
//! look at a message nor cheap to leak.
//!
//! # Sanitization is separate from redaction, and applies to everything
//!
//! Every string above goes through [`clean`]: control characters become
//! spaces, whitespace runs collapse, and the result is truncated. That is not
//! cosmetic. `from` and `rfc_message_id` are built from headers the *sender*
//! wrote, so without it a display name reading `Ada\n• Approved by finance\n•
//! Wire to acct 4455` renders in a Slack channel as three lines the operator
//! reads as rmail's own output, and an unbounded one becomes an unbounded
//! request body and an unbounded stored row. The redaction exemption below
//! covers *redaction only*; nothing is exempt from this.
//!
//! # `include_body` is one more field, and only for a destination that asked
//!
//! With `include_body` on for that destination (V48's column, off by
//! default), `message.body` carries the plain-text body, truncated to
//! [`MAX_BODY_CHARS`] and put through the redaction firewall like every other
//! content field. There is no global switch for this and no way for a *caller*
//! to turn it on: it is a property of the destination the operator
//! registered, because "this channel may see message bodies" is a statement
//! about a channel, not about a request.
//!
//! # AI enrichment carries what was already computed, and never spends
//!
//! `message.summary` and `message.action_items` are read out of
//! `ai_summaries` — the artifacts triage (task 49) and the deep pass already
//! produced and stored. Building a payload never calls a provider. That is
//! the same line [`crate::export`] draws for `with_ai`, and it matters twice
//! here: a dispatcher that could spend money per inbound message would be a
//! cost amplifier attached to an attacker-controlled trigger, and a delivery
//! *retry* that re-ran a model call would multiply the bill by the attempt
//! cap. `summary` is trimmed to two sentences ([`two_sentences`]) because
//! prd.md #64 asks for two sentences and a Slack message that spans a screen
//! gets muted.
//!
//! # Redaction: everything derived from message content, and one exemption
//!
//! `subject`, `body`, `summary` and each action item go through
//! [`crate::ai::redact::preview`] with the operator's own `[ai.privacy]`
//! settings before they are ever serialized — the same firewall, with the same
//! configuration, that governs text going to a model provider. A verification
//! code, a card number or an IBAN in a subject line does not reach a Slack
//! channel.
//!
//! What that firewall catches is the operator's own `ai.privacy.redact_patterns`
//! decision, not this module's. The shipped default is
//! `["ssn", "credit_card", "iban", "api_key", "otp"]` plus the four always-on
//! baseline kinds — so a *body* shipped to a `include_body` destination keeps
//! the third-party email addresses and phone numbers quoted inside it unless
//! the operator has widened that list. This is stated rather than quietly
//! assumed away: an operator turning `include_body` on is choosing to ship a
//! body, and the honest statement of what that means is "the same text a model
//! provider would see," not "everything identifying has been removed."
//!
//! `from` is the deliberate exemption *from redaction* (never from
//! sanitization — see above), and the reason is that it is not content — it is
//! the message's routing identity, and it is the single fact the notification
//! exists to convey. "Mail arrived from ⟦EMAIL_1⟧" tells the
//! operator nothing they could act on, so a payload that redacted it would be
//! a payload nobody would enable. The redaction firewall's `Email` kind exists
//! to stop *third-party* addresses quoted inside a body from being shipped
//! somewhere; the envelope sender of a message an operator explicitly asked to
//! be told about is not that. A destination that must not learn who writes to
//! this mailbox is a destination that must not be registered.
//!
//! # Slack rendering is text, and the text is escaped
//!
//! [`Template::Slack`] emits `{"text": ...}` — the one shape every Slack
//! incoming webhook accepts. Slack's `text` is mrkdwn, in which `&`, `<` and
//! `>` are control characters: a subject reading `<https://evil.example|click
//! here>` would render in the channel as a *link* whose visible text lies
//! about where it points. [`slack_escape`] converts those three to their HTML
//! entities exactly as Slack's own "Escaping text" documentation requires, so
//! attacker-controlled text renders as the characters it contains. This is
//! `crate::notify::channel`'s AppleScript concern in a different syntax and it
//! has the same answer: untrusted text is never allowed to become markup.

use serde_json::json;

use crate::ai::redact;
use crate::config::AiPrivacy;

use super::Template;

/// The longest body, in characters, a payload will carry when
/// `include_body` is on.
///
/// Well under `ai.privacy.max_body_chars` (40,000): a webhook body is
/// delivered to a chat channel or a ticket, not to a model with a context
/// window, and it is also stored verbatim in `webhook_deliveries.payload`
/// for every delivery ever made. Two thousand characters is several
/// paragraphs — enough to read the substance of a message — and bounds both
/// the request and the table.
pub const MAX_BODY_CHARS: usize = 2_000;

/// The longest subject a payload will carry.
pub const MAX_SUBJECT_CHARS: usize = 500;

/// The longest summary a payload will carry, after [`two_sentences`].
pub const MAX_SUMMARY_CHARS: usize = 1_000;

/// The longest sender a payload will carry. A display name plus an address
/// is well under this; anything longer is a sender padding the field.
pub const MAX_FROM_CHARS: usize = 320;

/// The longest RFC 5322 `Message-ID` a payload will carry.
pub const MAX_RFC_MESSAGE_ID_CHARS: usize = 512;

/// The most action items a payload will carry.
pub const MAX_ACTION_ITEMS: usize = 10;

/// The longest one action item may be.
pub const MAX_ACTION_ITEM_CHARS: usize = 300;

/// The URI scheme rmail addresses its own objects with.
///
/// `rmail://message/<local row id>` — the row id rather than the RFC 5322
/// `Message-ID`, because every rmail surface (`mail get <id>`,
/// `MailService/Get`, the TUI) is keyed by the row id, and a link a recipient
/// cannot paste into any of them is not a deep link. The RFC id travels
/// alongside it as `message.rfc_message_id` for anyone correlating against a
/// different mail store.
pub const LINK_SCHEME: &str = "rmail";

/// The deep link for a message.
#[must_use]
pub fn deep_link(message_id: i64) -> String {
    format!("{LINK_SCHEME}://message/{message_id}")
}

/// The facts about one message a payload may be built from, before
/// minimization and redaction.
///
/// Assembled by [`super::store`] from the database. Deliberately a plain
/// struct with no `Database` handle: a payload builder that could reach back
/// for a field it was not given would make `include_body` advisory rather
/// than structural.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageFacts {
    /// Local row id — what [`deep_link`] addresses.
    pub message_id: i64,
    /// The account's configured name.
    pub account: String,
    /// The mailbox the message is in.
    pub mailbox: String,
    /// The RFC 5322 `Message-ID`, bare, when the message has one.
    pub rfc_message_id: Option<String>,
    /// The sender, as `Name <addr>` or a bare address. Not redacted — see the
    /// module docs.
    pub from: String,
    /// The subject, raw. Redacted by [`build`].
    pub subject: String,
    /// The message date, unix seconds.
    pub date: Option<i64>,
    /// The plain-text body, raw. `None` unless the destination has
    /// `include_body` on; [`build`] never reads it otherwise, but it is only
    /// ever populated for such a destination in the first place.
    pub body: Option<String>,
    /// A stored Claude summary, if the AI passes have run on this message.
    pub summary: Option<String>,
    /// Stored action items, if any.
    pub action_items: Vec<String>,
}

/// Build the JSON body for one delivery.
///
/// `event` is the wire string a receiver routes on. `delivery_id` is the
/// stable id it dedupes on. `include_body` is the destination's own column,
/// passed explicitly rather than read off a config so that the one decision
/// about whether a body may leave is made at exactly one place and is visible
/// in this signature.
#[must_use]
pub fn build(
    template: Template,
    event: &str,
    delivery_id: i64,
    facts: &MessageFacts,
    include_body: bool,
    privacy: &AiPrivacy,
) -> serde_json::Value {
    let subject = clean(&redacted(&facts.subject, privacy), MAX_SUBJECT_CHARS);
    let summary = facts
        .summary
        .as_deref()
        .map(|s| clean(&redacted(&two_sentences(s), privacy), MAX_SUMMARY_CHARS))
        .filter(|s| !s.is_empty());
    let action_items: Vec<String> = facts
        .action_items
        .iter()
        .take(MAX_ACTION_ITEMS)
        .map(|item| clean(&redacted(item, privacy), MAX_ACTION_ITEM_CHARS))
        .filter(|item| !item.is_empty())
        .collect();
    // `include_body` gates the read, not just the write: a body that is never
    // read cannot be serialized by a later edit to this function.
    let body = if include_body {
        facts
            .body
            .as_deref()
            .map(|b| clean(&redacted(&truncate(b, MAX_BODY_CHARS), privacy), usize::MAX))
            .filter(|b| !b.is_empty())
    } else {
        None
    };
    // Not redacted (see the module docs) but emphatically still *sanitized*:
    // `from` is built from the message's own `From:` display name, which is
    // attacker-authored, unbounded, and — before this `clean` — could carry a
    // newline straight into the rendered Slack text and forge lines that read
    // as rmail's own output (a fake action item, a fake deep link). Redaction
    // and sanitization are different controls and only the first is exempted.
    let from = clean(&facts.from, MAX_FROM_CHARS);
    // The RFC 5322 `Message-ID` is a header the sender wrote, so it gets the
    // same treatment for the same reason. Bounded hard: a well-formed one is
    // tens of characters, and nothing downstream benefits from a longer one.
    let rfc_message_id = clean(
        facts.rfc_message_id.as_deref().unwrap_or_default(),
        MAX_RFC_MESSAGE_ID_CHARS,
    );
    let link = deep_link(facts.message_id);

    let mut message = serde_json::Map::new();
    message.insert("id".to_owned(), json!(facts.message_id));
    message.insert("link".to_owned(), json!(link));
    message.insert("from".to_owned(), json!(from));
    message.insert("subject".to_owned(), json!(subject));
    message.insert("account".to_owned(), json!(facts.account));
    message.insert("mailbox".to_owned(), json!(facts.mailbox));
    message.insert("date".to_owned(), json!(facts.date));
    message.insert("rfc_message_id".to_owned(), json!(rfc_message_id));
    if let Some(summary) = &summary {
        message.insert("summary".to_owned(), json!(summary));
    }
    if !action_items.is_empty() {
        message.insert("action_items".to_owned(), json!(action_items));
    }
    if let Some(body) = &body {
        message.insert("body".to_owned(), json!(body));
    }

    match template {
        Template::Generic => json!({
            "event": event,
            "delivery_id": delivery_id,
            "message": serde_json::Value::Object(message),
        }),
        // Slack requires `text`; the structured object rides alongside it
        // under our own key, which Slack ignores and a generic receiver
        // pointed at a `slack` destination can still read. Nothing is
        // duplicated into `text` that is not already in `message`.
        Template::Slack => json!({
            "text": slack_text(event, &from, &subject, summary.as_deref(), &action_items, &link),
            "event": event,
            "delivery_id": delivery_id,
            "message": serde_json::Value::Object(message),
        }),
    }
}

/// Render the Slack `text` field: who, what, the summary, the action items,
/// and the link — every untrusted fragment escaped (see the module docs).
fn slack_text(
    event: &str,
    from: &str,
    subject: &str,
    summary: Option<&str>,
    action_items: &[String],
    link: &str,
) -> String {
    let mut out = String::new();
    out.push('*');
    // `from` is the already-`clean`ed value, not `facts.from` — taking the raw
    // one here would reintroduce exactly the newline forgery the clean exists
    // to stop, in the one field that renders as prose.
    out.push_str(&slack_escape(from));
    out.push_str("* — ");
    out.push_str(&slack_escape(if subject.is_empty() {
        "(no subject)"
    } else {
        subject
    }));
    if let Some(summary) = summary {
        out.push('\n');
        out.push_str(&slack_escape(summary));
    }
    for item in action_items {
        out.push_str("\n• ");
        out.push_str(&slack_escape(item));
    }
    out.push('\n');
    // The link is rmail's own construction from an integer, not message
    // content, so it is the one fragment here with nothing to escape — but it
    // goes through the same function anyway rather than resting on that
    // remaining true.
    out.push_str(&slack_escape(link));
    out.push_str(" (");
    out.push_str(&slack_escape(event));
    out.push(')');
    out
}

/// Escape the three characters Slack's mrkdwn treats as control characters,
/// per Slack's "Escaping text" rules. See the module docs for why this is not
/// optional.
#[must_use]
pub fn slack_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            // `&` first is not an ordering accident: doing it later would
            // re-escape the ampersands this function itself just emitted.
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// The first two sentences of `text`, or all of it if it has fewer.
///
/// prd.md #64 asks for a two-sentence summary; the stored `ai_summaries`
/// artifacts are written for other consumers and are frequently longer, so
/// the trim happens here rather than by asking a model for a second, shorter
/// answer (which would spend money to shorten text already paid for).
///
/// A sentence ends at `.`, `!` or `?` followed by whitespace or end of text —
/// deliberately naive. Getting "Dr. Smith" wrong costs a slightly longer
/// Slack message; nothing here depends on the boundary being linguistically
/// correct.
#[must_use]
pub fn two_sentences(text: &str) -> String {
    let text = text.trim();
    let mut ends = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if !matches!(ch, '.' | '!' | '?') {
            continue;
        }
        let terminal = chars.peek().map_or(true, |(_, next)| next.is_whitespace());
        if !terminal {
            continue;
        }
        ends += 1;
        if ends == 2 {
            // `idx + ch.len_utf8()` is a char boundary by construction, so
            // this slice can never split a multi-byte sequence.
            return text[..idx + ch.len_utf8()].to_owned();
        }
    }
    text.to_owned()
}

/// Run `text` through the redaction firewall with the operator's own privacy
/// settings, returning exactly what would be sent.
///
/// With `ai.privacy.redact = false` this is `text` unchanged — the documented
/// opt-out, honestly reported (see [`crate::ai::redact::preview`]'s own docs).
fn redacted(text: &str, privacy: &AiPrivacy) -> String {
    if text.is_empty() {
        return String::new();
    }
    redact::preview(text, privacy).redacted_text
}

/// Collapse control characters to spaces, squeeze runs of whitespace, trim,
/// and truncate.
///
/// Control characters would otherwise ride into a chat channel or a ticket
/// title verbatim — a subject containing a carriage return can forge what
/// looks like a second line of a rendered message.
fn clean(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    let mut last_was_space = true;
    for ch in text.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if ch.is_whitespace() {
            if last_was_space {
                continue;
            }
            last_was_space = true;
            out.push(' ');
        } else {
            last_was_space = false;
            out.push(ch);
        }
    }
    let trimmed = out.trim_end();
    truncate(trimmed, max)
}

/// The first `max` characters of `text` — counted in characters, never bytes,
/// so this can never split a multi-byte sequence.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max).collect()
}
