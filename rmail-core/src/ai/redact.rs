//! The PII redaction firewall that every outbound model call passes through.
//!
//! # Where this sits
//!
//! ```text
//! Sync Engine ──▶ AI Queue ──▶ redact ──▶ Provider ──▶ audit ──▶ policy
//! ```
//!
//! [`guard`] is the pre-flight: it takes the [`ChatRequest`] the caller was
//! about to send, replaces every value it recognizes as sensitive with an
//! opaque token, and hands back either a request safe to pass to
//! [`Provider::complete`](crate::ai::provider::Provider::complete)/`stream`,
//! or [`GuardedRequest::RedactedSkip`] telling the caller not to call the
//! provider at all. This module never touches a
//! [`Provider`](crate::ai::provider::Provider) — it wraps the request
//! *before* one is called, exactly as the pipeline above and the `ai`
//! module docs describe. Nothing here changes what a `Provider` does with a
//! request; it only changes what request it receives.
//!
//! # Reversible means in memory, and only in memory
//!
//! [`TokenMap`] is a plain, unpersisted `HashMap`. It exists for the
//! lifetime of one request/response round trip: [`guard`] mints it,
//! [`rehydrate`] consumes it, and then it is dropped. Nothing here writes a
//! token mapping to disk, logs it, or hands it to the audit ledger (task
//! 45's job, and it records the *redacted* payload's hash — the whole point
//! of a firewall — not a way to reverse it). Persisting the map would turn
//! "the API never sees raw PII" into "the API never sees raw PII, but a
//! database row now lets anyone reconstruct it," which defeats the purpose.
//! [`TokenMap`]'s `Debug` impl deliberately does not print values, the same
//! discipline [`crate::credential::Secret`] applies to a resolved
//! credential — so an accidental `tracing::debug!(?tokens)` cannot leak
//! what this module exists to protect.
//!
//! # Reusing entity extraction, and where it stops
//!
//! Task 19's [`entities`](crate::index::entities) module already detects
//! emails, phones and IBANs with tested, checksum-verified span handling —
//! [`scan`] calls [`entities::scan`] once and keeps only the
//! [`EntityKind::Email`], [`EntityKind::Phone`] and [`EntityKind::Iban`]
//! mentions it returns, so this module carries no second copy of those
//! patterns. Its other kinds (URL, amount, date, tracking number,
//! order/invoice id) are not reused: none of them is PII, and folding them
//! into the token stream would redact useful, harmless context out of
//! every request for no privacy benefit.
//!
//! Cards, SSNs, secrets/API keys, one-time codes, postal addresses and
//! names have no entity-extraction equivalent and get their own detectors
//! here. The two modules deliberately optimize in opposite directions.
//! Entity extraction is precision-over-recall: a wrong entity pollutes a
//! permanent co-occurrence graph and every search that touches it, so it is
//! better to miss one than to invent one. A redaction firewall is the
//! mirror image — a false positive here just replaces some harmless text
//! with a token, which costs a little usefulness; a false negative lets raw
//! PII reach the model, which is the one failure this module exists to
//! rule out. So where entity extraction declines to claim anything it
//! cannot verify (the IBAN checksum, the carrier-shaped tracking number),
//! the detectors below lean the other way: a credit card is still checked
//! by Luhn (the acceptance criterion is explicit about that one), but the
//! name, address and secret patterns are intentionally broader than
//! `index::entities` would ever allow itself to be.
//!
//! # A URL can shadow the address inside it
//!
//! `index::entities::scan` resolves its own overlaps too, and a URL beats
//! an email/phone at the same span under its leftmost-first rule (see that
//! module's `scan_urls` docs) — the correct call for *its* purposes, since
//! a link is one thing, not a link plus a duplicate address entity. The
//! consequence for this module: an address embedded in a URL's path or
//! query — `https://x.com/unsubscribe?e=jane@example.com`, the single most
//! common shape in list, marketing and receipt mail — never appears as an
//! `EntityKind::Email` mention at all; [`entities::scan`] only ever hands
//! this module the `Url` mention. [`scan`] below closes that specific gap
//! without a second copy of `index::entities`' (private) email pattern: a
//! `Url` mention containing a raw `@` or a percent-encoded `%40` is
//! tokenized as a whole, under [`RedactionKind::Email`]. That is blunter
//! than tokenizing only the embedded address — the model loses the rest of
//! the link too — but this module's stated bias is exactly that trade.
//! Phone numbers and IBANs embedded in a URL are not covered by this same
//! check: unlike an `@`, there is no unambiguous signal in a URL that
//! distinguishes an embedded phone number from a tracking id or a
//! timestamp without risking the opposite mistake (blunt-tokenizing links
//! that carry no PII at all), so that residual gap is accepted and
//! documented rather than guessed at.
//!
//! # The system prompt is never scanned
//!
//! [`guard`] redacts every message's `content`, never [`ChatRequest::system`].
//! Two reasons, not one. First, the system prompt is authored by this
//! codebase, not derived from mail — there is nothing in it a redaction
//! pass could find that would be a user's PII. Second, and more concretely:
//! `provider.rs`'s prompt caching depends on the system prompt staying
//! byte-identical across calls so it can sit behind Anthropic's
//! `cache_control` boundary (see that module's docs). Scanning it here
//! would risk a token count that drifts between two calls carrying the
//! same literal prompt text for unrelated reasons, silently breaking the
//! cache hit rate this pipeline is built to depend on.
//!
//! # Token format
//!
//! A token looks like `⟦EMAIL_1⟧` — U+27E6/U+27E7, the mathematical white
//! square brackets, around a kind tag and a per-kind, per-request counter.
//! That bracket pair is picked specifically because it is not a character
//! anyone's keyboard produces by accident; an ASCII marker like `[EMAIL_1]`
//! risks collision with bracketed text a sender actually wrote (citation
//! markers, footnote references, `[edited]` annotations) in a way this
//! Unicode pair does not. The counter is per distinct *normalized* value,
//! not per occurrence: an email address mentioned five times in a thread
//! gets one token reused five times, not five different ones — treating
//! repeats as five different people would be wrong, and would make
//! [`GuardedRequest`]'s per-kind counts overstate how much distinct PII a
//! message actually contained.
//!
//! # Rehydration: exact match or verbatim passthrough, never a guess
//!
//! [`rehydrate`] looks for a token's exact literal text and substitutes the
//! value it stands for. A token the model echoes back reordered, wrapped in
//! markdown, or repeated works fine — [`rehydrate`] does not care where in
//! the text a token appears or how many times, only that its exact
//! characters are present. A token the model *mangles* — drops a character,
//! truncates the closing bracket, quotes only part of it — is a different
//! case: its text no longer matches anything in the [`TokenMap`], and this
//! module leaves it exactly as written rather than attempting a fuzzy
//! match. A wrong guess would substitute one person's address for
//! another's, which is worse than a user occasionally seeing `⟦EMAIL_1⟧` in
//! a response instead of the real address. This is a deliberate design
//! choice, not an oversight — see
//! `rehydrate_leaves_a_mangled_token_verbatim_rather_than_guessing` in the
//! tests for the case this decision covers.
//!
//! [`rehydrate`] operates on a complete string —
//! [`ChatResponse::text`](crate::ai::provider::ChatResponse) after a
//! non-streaming call, or a caller's own concatenation of every
//! [`StreamFrame::Token`](crate::ai::provider::StreamFrame) after a
//! streaming one, which `provider.rs`'s own docs note reproduces the final
//! text. A token placed right at a chunk boundary mid-stream is a real
//! possibility this module does not attempt to solve incrementally — that
//! is the same class of problem `SseDecoder` in `provider.rs` solves for
//! `\n\n` event boundaries, and a live, token-by-token rehydrating decoder
//! is follow-on work for whichever task streams a rehydrated response to a
//! UI, not something this module's scope covers today.
//!
//! # A sender can write a token-shaped string too
//!
//! A mail body is attacker-controlled, not just occasionally malformed —
//! nothing stops a sender from writing the literal text `⟦EMAIL_1⟧` and
//! hoping it collides with whatever this pass mints. Left alone, that would
//! do two bad things: [`has_residual_content`] treats token-shaped text as
//! not-really-content, so a message that is otherwise entirely forged
//! tokens could game its way to `redacted_skip`; and if this pass happens
//! to mint a real `⟦EMAIL_1⟧` of its own, [`rehydrate`] cannot tell the
//! forged occurrence from a genuine echo and substitutes the real value
//! into both. [`guard`] and [`preview`] both run
//! [`neutralize_preexisting_tokens`] over a message's content before
//! scanning it, which leaves the forged text's bytes visible (still
//! ordinary text to the model) while guaranteeing it can never again match
//! [`TOKEN_PATTERN`].
//!
//! # What "mandatory" means against a config toggle
//!
//! `ai.privacy.redact` is a real switch (see [`AiPrivacy`]), and [`guard`]
//! honors it: with it `false`, a request passes through unmodified. That is
//! not in tension with this firewall being "mandatory pre-flight" — the
//! task 47 AI queue calls [`guard`] unconditionally on every request before
//! it reaches a `Provider`, so redaction always runs; whether it *finds*
//! anything to redact is what the config controls, and turning it off is
//! the explicit, audited opt-out `prd.md` describes for a local-only
//! account, not a bypass of the pipeline stage itself.
//!
//! Within that, four kinds — email, phone, postal address and name — are
//! the always-on baseline whenever `redact` is `true`: they are not
//! individually listed in `ai.privacy.redact_patterns` because the
//! acceptance criteria treat them as non-optional. The remaining five
//! (card, IBAN, SSN, secret/API key, one-time code) track
//! `redact_patterns` by name, so an operator can narrow which of the more
//! specific categories apply.
//!
//! # `strip_attachments` and `max_body_chars` are not this module's job
//!
//! [`AiPrivacy`] also carries `strip_attachments` and `max_body_chars`.
//! Both are about *what content is assembled into a request* before this
//! firewall ever sees it — whether an attachment's extracted text is
//! included at all, and how much of a body is included before it is cut
//! off — which belongs to whatever builds the `ChatRequest` in the first
//! place (the task 47 queue). This module's job starts once that text
//! already exists; it does not decide what earns a place in the request,
//! only what it looks like once it's in there.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::{LazyLock, Mutex, PoisonError};

use regex::Regex;

use crate::ai::provider::{ChatMessage, ChatRequest};
use crate::config::AiPrivacy;
use crate::index::entities::{self, EntityKind};

/// Opens a redaction token. See the module docs for why this bracket pair
/// specifically.
const TOKEN_OPEN: char = '⟦';
/// Closes a redaction token.
const TOKEN_CLOSE: char = '⟧';

/// The longest single message content, in bytes, this pass will scan before
/// it starts dropping the remainder.
///
/// Unlike `index::entities`' `MAX_SCAN_BYTES` — where the failure mode of
/// giving up early is "some entities are missed, search degrades a little"
/// — giving up early here without also removing the unscanned tail would
/// mean raw, unscanned text reaches [`ChatRequest::messages`] and from
/// there a `Provider`. So [`bounded`] does not just stop scanning past this
/// limit, it truncates the text itself; nothing past the cut ever becomes
/// part of what [`guard`] hands back. In ordinary operation this is never
/// reached — `ai.privacy.max_body_chars` (default 40,000, measured in
/// characters) is applied upstream, well under this even accounting for
/// multi-byte UTF-8 — this exists as a second, independent bound in case
/// that upstream discipline is ever missing or misconfigured, since this
/// module's one job is to guarantee no raw PII leaves regardless of what
/// handed it the text.
const MAX_SCAN_BYTES: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Redaction kinds
// ---------------------------------------------------------------------------

/// One category of sensitive text the firewall knows how to find.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RedactionKind {
    /// An email address (via [`entities::scan`]).
    Email,
    /// A telephone number (via [`entities::scan`]).
    Phone,
    /// An international bank account number (via [`entities::scan`]).
    Iban,
    /// A payment card number, verified by Luhn.
    Card,
    /// A US Social Security Number, verified against SSA-issued ranges.
    Ssn,
    /// An API key, access token, bearer token or JWT — or a value
    /// explicitly labeled `api_key`/`secret`/`token`/`password` in prose.
    Secret,
    /// A one-time code: an OTP, verification code or PIN.
    Otp,
    /// A postal street address.
    Address,
    /// A person's name, found at a salutation, a sign-off, an email
    /// display name, or a self-introduction ("my name is …").
    Name,
}

impl RedactionKind {
    /// Every kind this module produces.
    pub const ALL: [Self; 9] = [
        Self::Email,
        Self::Phone,
        Self::Iban,
        Self::Card,
        Self::Ssn,
        Self::Secret,
        Self::Otp,
        Self::Address,
        Self::Name,
    ];

    /// The tag minted into this kind's tokens, e.g. `EMAIL` in `⟦EMAIL_1⟧`.
    fn token_tag(self) -> &'static str {
        match self {
            Self::Email => "EMAIL",
            Self::Phone => "PHONE",
            Self::Iban => "IBAN",
            Self::Card => "CARD",
            Self::Ssn => "SSN",
            Self::Secret => "SECRET",
            Self::Otp => "OTP",
            Self::Address => "ADDRESS",
            Self::Name => "NAME",
        }
    }

    /// The name this kind is enabled under in
    /// [`AiPrivacy::redact_patterns`], or `None` for one of the four
    /// always-on baseline kinds — see the module docs.
    fn pattern_name(self) -> Option<&'static str> {
        match self {
            Self::Card => Some("credit_card"),
            Self::Iban => Some("iban"),
            Self::Ssn => Some("ssn"),
            Self::Secret => Some("api_key"),
            Self::Otp => Some("otp"),
            Self::Email | Self::Phone | Self::Address | Self::Name => None,
        }
    }
}

/// Which kinds a pass should look for, given `privacy`.
///
/// Empty when `privacy.redact` is `false` — the documented opt-out, not a
/// bug. Otherwise the four baseline kinds plus whatever
/// `privacy.redact_patterns` names; an unrecognized name is logged (once —
/// see [`warn_unknown_pattern_once`]) and ignored rather than rejected
/// outright, since a config file naming a pattern this build does not know
/// is far more likely to be a version skew than a typo worth failing
/// startup over.
fn enabled_kinds(privacy: &AiPrivacy) -> BTreeSet<RedactionKind> {
    let mut enabled = BTreeSet::new();
    if !privacy.redact {
        return enabled;
    }
    enabled.insert(RedactionKind::Email);
    enabled.insert(RedactionKind::Phone);
    enabled.insert(RedactionKind::Address);
    enabled.insert(RedactionKind::Name);
    for pattern in &privacy.redact_patterns {
        match RedactionKind::ALL
            .into_iter()
            .find(|k| k.pattern_name() == Some(pattern.as_str()))
        {
            Some(kind) => {
                enabled.insert(kind);
            }
            None => warn_unknown_pattern_once(pattern),
        }
    }
    enabled
}

/// Log an unrecognized `ai.privacy.redact_patterns` entry, once per distinct
/// name for the life of the process.
///
/// [`enabled_kinds`] runs on every [`guard`]/[`preview`] call — every
/// outbound AI request, potentially thousands a day. Without deduplication
/// a single config typo would `warn!` once per request forever, which is
/// exactly the kind of log noise that makes a real warning easy to miss.
/// A `Mutex<HashSet<String>>` rather than something lock-free: this path is
/// taken at most once per distinct typo per process lifetime, so contention
/// is not a concern worth a more complex structure for.
fn warn_unknown_pattern_once(pattern: &str) {
    static WARNED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    let mut warned = WARNED.lock().unwrap_or_else(PoisonError::into_inner);
    let seen = warned.get_or_insert_with(HashSet::new);
    if seen.insert(pattern.to_owned()) {
        tracing::warn!(
            pattern,
            "unknown name in ai.privacy.redact_patterns; ignored"
        );
    }
}

// ---------------------------------------------------------------------------
// TokenMap
// ---------------------------------------------------------------------------

/// Reverses one firewall pass's tokenization: every token minted, mapped
/// back to the real value it stands in for.
///
/// In-memory only — see the module docs for why persisting this would
/// defeat the point of a firewall.
#[derive(Clone, Default)]
pub struct TokenMap(HashMap<String, String>);

impl TokenMap {
    /// Whether this pass found nothing to redact.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many distinct values this pass tokenized.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    fn insert(&mut self, token: String, value: String) {
        self.0.insert(token, value);
    }

    fn get(&self, token: &str) -> Option<&String> {
        self.0.get(token)
    }
}

/// Deliberately does not print values — the same discipline
/// [`crate::credential::Secret`] applies, so an accidental `{:?}` in a log
/// line cannot leak what this module exists to keep out of one.
impl fmt::Debug for TokenMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TokenMap({} token(s))", self.0.len())
    }
}

// ---------------------------------------------------------------------------
// guard(): the pre-flight over a ChatRequest
// ---------------------------------------------------------------------------

/// What running the firewall over one [`ChatRequest`] produced.
pub enum GuardedRequest {
    /// `request` is safe to hand to `Provider::complete`/`stream`; `tokens`
    /// reverses whatever it echoes back via [`rehydrate`]. `counts` is how
    /// many distinct values of each kind were tokenized — zero entries is a
    /// normal, common outcome, since most mail carries no PII at all.
    Redacted {
        /// The request to actually send.
        request: ChatRequest,
        /// Reverses the tokenization in a later response.
        tokens: TokenMap,
        /// Distinct values tokenized, by kind.
        counts: BTreeMap<RedactionKind, usize>,
    },
    /// Every message's content became empty (no alphanumeric text left)
    /// once its PII was replaced with tokens — there was nothing left
    /// worth a model call. The caller must record this as `redacted_skip`
    /// (`prd.md`'s AI queue section) and must not call the provider.
    RedactedSkip,
}

/// Deliberately hand-written rather than derived, and deliberately never
/// prints `request`'s message bodies. `ChatRequest` derives a normal
/// `Debug` that prints every message verbatim — fine for `provider.rs`,
/// where by the time a caller holds one it has already been through this
/// firewall, but `GuardedRequest::Redacted.request` is *also* what
/// `ai.privacy.redact = false` (a real, documented opt-out — see the
/// module docs) hands back completely unredacted. A `Redacted` variant name
/// reads as "safe," and a stray `tracing::debug!(?guarded)` written on that
/// assumption must not become the leak this whole module exists to
/// prevent. `counts` prints in full — kind names and quantities, never
/// values, the same safe-to-log shape [`TokenMap`]'s own `Debug` sticks to.
impl fmt::Debug for GuardedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redacted {
                request,
                tokens,
                counts,
            } => f
                .debug_struct("GuardedRequest::Redacted")
                .field("messages", &request.messages.len())
                .field("tokens", tokens)
                .field("counts", counts)
                .finish(),
            Self::RedactedSkip => f.write_str("GuardedRequest::RedactedSkip"),
        }
    }
}

/// The mandatory pre-flight: replace every value this module recognizes as
/// sensitive in `request`'s messages with an opaque token, so a `Provider`
/// never receives it.
///
/// `request.system` is never scanned — see the module docs. `request.model`,
/// `max_tokens` and `output_format` pass through unchanged.
#[must_use]
#[tracing::instrument(skip(request, privacy), fields(model = %request.model, messages = request.messages.len(), outcome, tokenized))]
pub fn guard(request: &ChatRequest, privacy: &AiPrivacy) -> GuardedRequest {
    let enabled = enabled_kinds(privacy);
    if enabled.is_empty() {
        tracing::Span::current().record("outcome", "passthrough");
        return GuardedRequest::Redacted {
            request: request.clone(),
            tokens: TokenMap::default(),
            counts: BTreeMap::new(),
        };
    }

    let mut tokens = TokenMap::default();
    let mut dedup: HashMap<(RedactionKind, String), String> = HashMap::new();
    let mut counts: BTreeMap<RedactionKind, usize> = BTreeMap::new();
    let mut any_residual_content = false;
    let mut redacted_messages = Vec::with_capacity(request.messages.len());

    for message in &request.messages {
        let content = neutralize_preexisting_tokens(bounded(&message.content));
        let hits = scan(&content, &enabled);
        let redacted_text = apply(&content, &hits, &mut dedup, &mut counts, &mut tokens);
        any_residual_content |= has_residual_content(&redacted_text);
        redacted_messages.push(ChatMessage {
            role: message.role,
            content: redacted_text,
        });
    }

    let span = tracing::Span::current();
    span.record("tokenized", tokens.len());
    if !any_residual_content {
        span.record("outcome", "redacted_skip");
        return GuardedRequest::RedactedSkip;
    }
    span.record("outcome", "redacted");

    // Built field-by-field rather than `request.clone()` then overwriting
    // `.messages`: the discarded clone of `.messages` would otherwise copy
    // every original (pre-redaction) message body — up to
    // `ai.privacy.max_body_chars` per message — for nothing.
    let redacted_request = ChatRequest {
        model: request.model.clone(),
        max_tokens: request.max_tokens,
        system: request.system.clone(),
        messages: redacted_messages,
        output_format: request.output_format.clone(),
    };
    GuardedRequest::Redacted {
        request: redacted_request,
        tokens,
        counts,
    }
}

/// Whether `redacted_text` still has anything worth sending, once its token
/// placeholders are disregarded.
///
/// "Anything worth sending" means at least one alphanumeric character
/// outside a token — punctuation and whitespace left behind by a body that
/// was entirely PII do not count. This is deliberately literal rather than
/// an attempt to detect "this is just a signature block": the acceptance
/// criterion is "empty after redaction", and a message that is *only* a
/// phone number or *only* an email address is the clear, unambiguous case
/// it describes. A message that mixes PII with other prose (even a short
/// label like "Contact:") is judged to have content worth sending, which
/// keeps this check simple and its behavior predictable rather than trying
/// to guess at what counts as "real" content.
fn has_residual_content(redacted_text: &str) -> bool {
    let Some(re) = TOKEN_PATTERN.as_ref() else {
        return redacted_text.chars().any(char::is_alphanumeric);
    };
    let mut cursor = 0usize;
    for m in re.find_iter(redacted_text) {
        if redacted_text
            .get(cursor..m.start())
            .unwrap_or_default()
            .chars()
            .any(char::is_alphanumeric)
        {
            return true;
        }
        cursor = m.end();
    }
    redacted_text
        .get(cursor..)
        .unwrap_or_default()
        .chars()
        .any(char::is_alphanumeric)
}

// ---------------------------------------------------------------------------
// preview(): the redact_preview surface
// ---------------------------------------------------------------------------

/// What sending `text` through the firewall would produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactPreview {
    /// Exactly the text that would be sent — tokens in place of PII, with
    /// nothing raw left in it. This is what a `redact_preview` caller
    /// should show: the real, wire-shape payload, not a summary of it.
    pub redacted_text: String,
    /// Distinct values tokenized, by kind.
    pub counts: BTreeMap<RedactionKind, usize>,
    /// Whether this text would short-circuit to `redacted_skip` instead of
    /// actually being sent.
    pub would_skip: bool,
}

/// Preview what running the firewall over `text` would send, without
/// requiring a [`ChatRequest`] or returning a [`TokenMap`] — this is an
/// audit surface, not a way to send anything. A caller that actually wants
/// to make a call still goes through [`guard`], whose token map is required
/// to make sense of the response.
///
/// With `privacy.redact` `false`, `redacted_text` is `text` unchanged — a
/// faithful preview of "what would be sent" when nothing is being redacted
/// at all, not a bug. It does mean this is not a type to log wholesale in
/// that configuration; see [`GuardedRequest`]'s `Debug` impl for the same
/// concern on the call path this previews.
#[must_use]
pub fn preview(text: &str, privacy: &AiPrivacy) -> RedactPreview {
    let enabled = enabled_kinds(privacy);
    let content = neutralize_preexisting_tokens(bounded(text));
    let hits = scan(&content, &enabled);
    let mut tokens = TokenMap::default();
    let mut dedup: HashMap<(RedactionKind, String), String> = HashMap::new();
    let mut counts: BTreeMap<RedactionKind, usize> = BTreeMap::new();
    let redacted_text = apply(&content, &hits, &mut dedup, &mut counts, &mut tokens);
    let would_skip = !has_residual_content(&redacted_text);
    RedactPreview {
        redacted_text,
        counts,
        would_skip,
    }
}

// ---------------------------------------------------------------------------
// rehydrate(): turning tokens back into real values
// ---------------------------------------------------------------------------

/// Recognizes a token's shape in a model response. Matching the shape and
/// then looking the literal text up in the [`TokenMap`] (rather than, say,
/// parsing out the kind and index and trusting them) means a token this
/// pass never minted — hand-typed by the model, or left over from a
/// different request entirely — is never mistaken for one that maps to a
/// real value.
static TOKEN_PATTERN: LazyLock<Option<Regex>> = LazyLock::new(|| compile(r"⟦[A-Z]+_[0-9]+⟧"));

/// Turn a model response's text back into one with real values in place of
/// whatever tokens it echoes — so the user sees real data even though the
/// model never did.
///
/// Every exact occurrence of a token this `tokens` map knows is replaced,
/// however many times it appears and in whatever order. Token-shaped text
/// this map does not recognize (a mangled token, a partial quote, one from
/// an unrelated request) is left exactly as written — see the module docs
/// for why guessing at it would be worse.
#[must_use]
pub fn rehydrate(text: &str, tokens: &TokenMap) -> String {
    if tokens.is_empty() {
        return text.to_owned();
    }
    let Some(re) = TOKEN_PATTERN.as_ref() else {
        return text.to_owned();
    };
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for m in re.find_iter(text) {
        out.push_str(text.get(cursor..m.start()).unwrap_or_default());
        match tokens.get(m.as_str()) {
            Some(value) => out.push_str(value),
            None => out.push_str(m.as_str()),
        }
        cursor = m.end();
    }
    out.push_str(text.get(cursor..).unwrap_or_default());
    out
}

/// Neutralize any token-shaped text already present in `text`, before this
/// pass mints its own tokens — see the module docs' "A sender can write a
/// token-shaped string too" section for why this runs at all.
///
/// Every match of [`TOKEN_PATTERN`] has the token brackets is its first and
/// last character by construction; swapping them for plain ASCII brackets
/// leaves the tag and number intact (still ordinary, readable text) while
/// guaranteeing the result can never match [`TOKEN_PATTERN`] again. Returns
/// `text` unchanged, allocation-free, when nothing matches — the common
/// case for ordinary mail.
fn neutralize_preexisting_tokens(text: &str) -> Cow<'_, str> {
    let Some(re) = TOKEN_PATTERN.as_ref() else {
        return Cow::Borrowed(text);
    };
    if !re.is_match(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for m in re.find_iter(text) {
        out.push_str(text.get(cursor..m.start()).unwrap_or_default());
        let mut inner = m.as_str().chars();
        inner.next(); // the opening token bracket
        inner.next_back(); // the closing token bracket
        out.push('[');
        out.push_str(inner.as_str());
        out.push(']');
        cursor = m.end();
    }
    out.push_str(text.get(cursor..).unwrap_or_default());
    Cow::Owned(out)
}

// ---------------------------------------------------------------------------
// Scanning and tokenizing
// ---------------------------------------------------------------------------

/// One thing found worth redacting.
#[derive(Debug, Clone)]
struct Hit {
    kind: RedactionKind,
    start: usize,
    end: usize,
    /// As written — becomes the value a token maps back to.
    value: String,
    /// Normalized identity — what two spellings of the same value share, so
    /// they get one token rather than two.
    key: String,
}

/// Run every enabled detector over `text` and resolve overlaps.
fn scan(text: &str, enabled: &BTreeSet<RedactionKind>) -> Vec<Hit> {
    let mut hits = Vec::new();

    if enabled.contains(&RedactionKind::Email)
        || enabled.contains(&RedactionKind::Phone)
        || enabled.contains(&RedactionKind::Iban)
    {
        for mention in entities::scan(text) {
            let kind = match mention.kind {
                EntityKind::Email => Some(RedactionKind::Email),
                EntityKind::Phone => Some(RedactionKind::Phone),
                EntityKind::Iban => Some(RedactionKind::Iban),
                // See the module docs' "A URL can shadow the address inside
                // it": `index::entities` resolves a URL over an email/phone
                // mention at the same span, so this is the only place this
                // module ever gets a chance to notice one was there.
                EntityKind::Url if url_embeds_email(&mention.value) => Some(RedactionKind::Email),
                // Not PII, or not this module's concern — see the module
                // docs on why URL/amount/date/tracking/order/invoice are
                // never tokenized.
                _ => None,
            };
            let Some(kind) = kind else { continue };
            if enabled.contains(&kind) {
                hits.push(Hit {
                    kind,
                    start: mention.span_start,
                    end: mention.span_end,
                    value: mention.value,
                    key: mention.norm,
                });
            }
        }
    }
    if enabled.contains(&RedactionKind::Card) {
        hits.extend(scan_cards(text));
    }
    if enabled.contains(&RedactionKind::Ssn) {
        hits.extend(scan_ssns(text));
    }
    if enabled.contains(&RedactionKind::Secret) {
        hits.extend(scan_secrets(text));
    }
    if enabled.contains(&RedactionKind::Otp) {
        hits.extend(scan_otps(text));
    }
    if enabled.contains(&RedactionKind::Address) {
        hits.extend(scan_addresses(text));
    }
    if enabled.contains(&RedactionKind::Name) {
        hits.extend(scan_names(text));
    }

    resolve_overlaps(hits)
}

/// Resolve overlapping hits by keeping the one that starts earlier and, on a
/// tie, the longer one — the same precedence `index::entities::scan`
/// applies, so two detectors that both claim the same span (a labeled
/// secret whose value happens to be card-shaped, say) agree on one winner.
///
/// A hit that overlaps the kept one *extends* it rather than being dropped
/// outright: `last.end` grows to cover whichever end reaches further. Simply
/// discarding the loser — this function's first version — left a gap
/// whenever the loser's span reached past the winner's, and nothing after
/// that point in `apply` ever gets replaced by a token: the loser's own tail
/// would have gone out raw. Extending the kept span means every byte either
/// hit touched ends up on one side of the union or the other, never in a
/// dropped middle. The identity (`kind`/`value`/`key`) stays the earlier
/// hit's — only the covered range changes — which is a defensible attribution
/// (the earlier detector matched first) and, more importantly, is not the
/// property this exists to protect: no raw text in the union survives past
/// [`apply`] either way.
fn resolve_overlaps(mut hits: Vec<Hit>) -> Vec<Hit> {
    hits.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)));
    let mut kept: Vec<Hit> = Vec::with_capacity(hits.len());
    for hit in hits {
        match kept.last_mut() {
            Some(last) if hit.start < last.end => {
                if hit.end > last.end {
                    last.end = hit.end;
                }
            }
            _ => kept.push(hit),
        }
    }
    kept
}

/// Whether `url` — as `index::entities` classified it, ahead of an
/// email/phone/IBAN mention at the same span (see the module docs' "A URL
/// can shadow the address inside it") — carries an embedded email address.
///
/// Checked by substring rather than re-implementing `index::entities`'
/// (private) email pattern here: this module's own docs already argue
/// against a second copy of those patterns, and a URL is tokenized whole
/// once this returns `true` anyway (see [`scan`]), so nothing downstream
/// needs the address's exact span within the URL, only that one is there.
/// `%40` catches the common case of a percent-encoded address in a query
/// string, which would not match `index::entities`' email pattern even
/// where it not shadowed by the URL.
fn url_embeds_email(url: &str) -> bool {
    url.contains('@') || url.to_ascii_lowercase().contains("%40")
}

/// Replace every hit in `text` with a token, minting a new one only for a
/// `(kind, key)` not already seen in `dedup` — so repeated mentions of the
/// same value across a whole request share one token. `hits` must already
/// be sorted ascending by `start` and non-overlapping, which
/// [`resolve_overlaps`] guarantees.
fn apply(
    text: &str,
    hits: &[Hit],
    dedup: &mut HashMap<(RedactionKind, String), String>,
    counts: &mut BTreeMap<RedactionKind, usize>,
    tokens: &mut TokenMap,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for hit in hits {
        out.push_str(text.get(cursor..hit.start).unwrap_or_default());
        let dedup_key = (hit.kind, hit.key.clone());
        let token = if let Some(existing) = dedup.get(&dedup_key) {
            existing.clone()
        } else {
            let n = counts.entry(hit.kind).or_insert(0);
            *n += 1;
            let minted = format!("{TOKEN_OPEN}{}_{n}{TOKEN_CLOSE}", hit.kind.token_tag());
            tokens.insert(minted.clone(), hit.value.clone());
            dedup.insert(dedup_key, minted.clone());
            minted
        };
        out.push_str(&token);
        cursor = hit.end;
    }
    out.push_str(text.get(cursor..).unwrap_or_default());
    out
}

/// Bound how much of `text` a scan will look at — see [`MAX_SCAN_BYTES`].
/// Unlike a plain byte truncation, this walks back to a valid `char`
/// boundary first, the same discipline `provider.rs`'s `clip` applies to an
/// upstream error body.
fn bounded(text: &str) -> &str {
    if text.len() <= MAX_SCAN_BYTES {
        return text;
    }
    let mut end = MAX_SCAN_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    tracing::warn!(
        bytes = text.len(),
        limit = MAX_SCAN_BYTES,
        "message exceeds the redaction scan budget; truncating before it can reach a Claude call"
    );
    text.get(..end).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Detectors with no `index::entities` equivalent
// ---------------------------------------------------------------------------

/// A run of 13-19 digits, optionally grouped with whitespace, dashes or
/// dots — payment card numbers are never longer than 19 digits (the
/// ISO/IEC 7812 maximum) or shorter than 13 (the shortest issued today,
/// some Maestro cards). Claimed only once Luhn agrees; see [`luhn_valid`].
///
/// The separator sits *between* two digits (`\d(?:[\s.\-]?\d){12,18}`), not
/// after `\d` as `(?:\d[\s.\-]?){13,19}` would put it: with the separator
/// trailing, greedy matching happily consumes a real space that comes right
/// after the number and before the next word — "4111 1111 1111 1111 ok"
/// matched through the trailing "1111 " into "ok", because `\b` only checks
/// for a word/non-word transition and a space-then-letter is exactly that.
/// Anchoring every separator between two digits makes that impossible: there
/// is no repetition slot left to consume a trailing one.
///
/// `\s`, not a literal space: HTML-derived bodies routinely carry U+00A0
/// (non-breaking space) where a plain space would visually appear, and
/// `\s` in this crate's default Unicode mode matches it — a literal `[ -]`
/// would silently pass an NBSP-grouped card number through unredacted. Luhn
/// still has to agree before any of this is claimed, so widening the
/// separator class does not trade away precision the way it might for an
/// unchecked pattern.
static CARD: LazyLock<Option<Regex>> = LazyLock::new(|| compile(r"\b\d(?:[\s.\-]?\d){12,18}\b"));

fn scan_cards(text: &str) -> Vec<Hit> {
    let Some(re) = CARD.as_ref() else {
        return Vec::new();
    };
    re.find_iter(text)
        .filter_map(|m| {
            let digits: String = m.as_str().chars().filter(char::is_ascii_digit).collect();
            if !(13..=19).contains(&digits.len()) || !luhn_valid(&digits) {
                return None;
            }
            Some(Hit {
                kind: RedactionKind::Card,
                start: m.start(),
                end: m.end(),
                value: m.as_str().to_owned(),
                key: digits,
            })
        })
        .collect()
}

/// The Luhn checksum: double every second digit counting from the
/// rightmost (the check digit itself is never doubled), subtract 9 from any
/// result over 9, and the total must be a multiple of 10. Without this, the
/// card pattern above matches any 13-19 digit run, and a mailbox has plenty
/// of those that are not card numbers (order references, phone numbers with
/// their separators stripped, tracking fragments).
fn luhn_valid(digits: &str) -> bool {
    let values: Vec<u32> = digits.chars().filter_map(|c| c.to_digit(10)).collect();
    if values.len() < 12 {
        return false;
    }
    let sum: u32 = values
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let doubled = d * 2;
                if doubled > 9 {
                    doubled - 9
                } else {
                    doubled
                }
            } else {
                d
            }
        })
        .sum();
    // `sum > 0` excludes an all-zero run, which passes the arithmetic
    // trivially but is not a card number anyone was issued.
    sum > 0 && sum % 10 == 0
}

/// A US Social Security Number's wire shape: `NNN-NN-NNNN`.
static SSN: LazyLock<Option<Regex>> = LazyLock::new(|| compile(r"\b(\d{3})-(\d{2})-(\d{4})\b"));

fn scan_ssns(text: &str) -> Vec<Hit> {
    let Some(re) = SSN.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(text)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            let area: u32 = caps.get(1)?.as_str().parse().ok()?;
            let group = caps.get(2)?.as_str();
            let serial = caps.get(3)?.as_str();
            // SSA never issued area 000, 666, or 900-999, group 00, or
            // serial 0000. A pattern that claimed every `\d{3}-\d{2}-\d{4}`
            // run would also claim plenty of dates, order suffixes and
            // version strings that happen to share the shape.
            if area == 0 || area == 666 || area >= 900 || group == "00" || serial == "0000" {
                return None;
            }
            let digits: String = whole
                .as_str()
                .chars()
                .filter(char::is_ascii_digit)
                .collect();
            Some(Hit {
                kind: RedactionKind::Ssn,
                start: whole.start(),
                end: whole.end(),
                value: whole.as_str().to_owned(),
                key: digits,
            })
        })
        .collect()
}

/// Recognizable secret/token shapes, plus a generic `label: value` fallback
/// for the many bespoke formats those specific shapes miss. The specific
/// alternatives are checked first (longest-match concerns do not apply here
/// the way they do in `index::entities`' leftmost-first patterns, since
/// these shapes do not overlap each other) so a well-known prefix is
/// claimed by its own rule rather than by the generic label rule
/// mis-splitting it.
static SECRET: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(
        r#"(?x)
        \b sk-ant-[A-Za-z0-9_-]{20,} \b        # Anthropic API key
      | \b sk-[A-Za-z0-9_-]{20,} \b            # OpenAI-style secret key
      | \b AKIA[0-9A-Z]{16} \b                 # AWS access key id
      | \b gh[pousr]_[A-Za-z0-9]{36,} \b       # GitHub personal/app token
      | \b eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,} \b  # JWT
      | (?i:api[_-]?key|secret|token|password|passwd|pwd)
        \s*[:=]\s*"?(?P<labeled>[A-Za-z0-9\-_./+]{6,})"?
        "#,
    )
});

fn scan_secrets(text: &str) -> Vec<Hit> {
    let Some(re) = SECRET.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(text)
        .filter_map(|caps| {
            // The labeled fallback redacts only the value, not the label —
            // "password: hunter2xyz" becomes "password: ⟦SECRET_1⟧", the
            // same span-covers-the-identifier-not-the-sentence rule
            // `index::entities`' reference pattern applies.
            let m = caps.name("labeled").or_else(|| caps.get(0))?;
            Some(Hit {
                kind: RedactionKind::Secret,
                start: m.start(),
                end: m.end(),
                value: m.as_str().to_owned(),
                key: m.as_str().to_owned(),
            })
        })
        .collect()
}

/// A one-time code: anchored on a nearby word the same way
/// `index::entities`' tracking-number and reference patterns are — a bare
/// 4-8 digit run is far too common (years, small order counts, partial
/// phone numbers) to claim unaided.
///
/// The connector between the trigger phrase and the digits is
/// `(?:is\s*)?:?\s*`, not `(?:is|:)?\s*` — the earlier version treated "is"
/// and ":" as alternatives, so it could consume one or the other but never
/// both, and "Your verification code is: 483920" (a linking verb *and* a
/// colon, an entirely ordinary phrasing) did not match. `code`/`pin` are
/// bare triggers too, not just `verification code`/`PIN code`: "Your code
/// is 123456" and "Your PIN is 4821" are at least as common as the more
/// specific phrasings already covered, and a bare digit run is still
/// required to be *preceded* by one of these words, so this does not widen
/// the false-positive surface the module docs already accept, only closes
/// an anchor gap. `\**` around the digits tolerates a markdown-bolded code
/// (`**123456**`), which is what an HTML-derived body's plain-text
/// extraction commonly leaves behind.
///
/// `[-\ ]`, not `[- ]`: under `(?x)`/`(?ix)` extended mode, the `regex`
/// crate ignores whitespace even *inside* a character class unless it is
/// escaped — `[- ]` silently becomes just `[-]`, so "one time code" (the
/// space variant) stopped matching while "one-time code" kept working, and
/// nothing about that failure looks like a compile error or a `None` from
/// [`compile`]; it just quietly drops half the alternation. Escaping the
/// space (`\ `) is what actually keeps it in the class.
static OTP: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(
        r"(?ix)
        (?: one[-\ ]?time\ (?:code|password|passcode)
          | verification\ code
          | security\ code
          | auth(?:entication)?\ code
          | passcode
          | OTP
          | PIN(?:\ (?:code|number))?
          | code
        )
        (?:\s*is)?\s*:?\s*
        \**(?P<code>[0-9]{4,8})\**\b",
    )
});

fn scan_otps(text: &str) -> Vec<Hit> {
    let Some(re) = OTP.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(text)
        .filter_map(|caps| {
            let code = caps.name("code")?;
            Some(Hit {
                kind: RedactionKind::Otp,
                start: code.start(),
                end: code.end(),
                value: code.as_str().to_owned(),
                key: code.as_str().to_owned(),
            })
        })
        .collect()
}

/// A US-style street address: a house number, one to five capitalized
/// words, then a recognized street suffix, with optional unit and
/// city/state/ZIP. Deliberately narrow and US-centric — this is the one
/// detector in this module with no checksum or carrier shape to lean on, so
/// it stays anchored on the street suffix the same way `index::entities`'
/// reference pattern stays anchored on a label, rather than trying to
/// recognize a postal address from shape alone.
const STREET_SUFFIX: &str = r"(?:Street|St|Avenue|Ave|Boulevard|Blvd|Drive|Dr|Lane|Ln|Road|Rd|Court|Ct|Place|Pl|Way|Circle|Cir|Terrace|Ter|Square|Sq|Highway|Hwy|Parkway|Pkwy)";

static ADDRESS: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(&format!(
        r"(?x)
        \b[0-9]{{1,6}}\s+
        [A-Z][A-Za-z0-9.'-]*(?:\s+[A-Z][A-Za-z0-9.'-]*){{0,4}}\s+{STREET_SUFFIX}\b\.?
        (?:\s*,?\s*(?:Apt|Suite|Ste|Unit|\#)\.?\s*[A-Za-z0-9-]+)?
        (?:\s*,\s*[A-Z][A-Za-z]+(?:\s+[A-Z][A-Za-z]+)*\s*,\s*[A-Z]{{2}}\s+[0-9]{{5}}(?:-[0-9]{{4}})?\b)?
        "
    ))
});

fn scan_addresses(text: &str) -> Vec<Hit> {
    let Some(re) = ADDRESS.as_ref() else {
        return Vec::new();
    };
    re.find_iter(text)
        .map(|m| {
            let key = m
                .as_str()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            Hit {
                kind: RedactionKind::Address,
                start: m.start(),
                end: m.end(),
                value: m.as_str().to_owned(),
                key,
            }
        })
        .collect()
}

/// A person's name, anchored on one of five signals: a salutation ("Dear
/// John,"), a sign-off ("Regards,\nJohn" or the same-line "Thanks, Jane"),
/// an email display name in either its bare ("John Smith
/// <john@x.com>") or RFC 5322 quoted form ("\"Jane Doe\"
/// <jane@x.com>" — the form required whenever a display name contains a
/// comma or period, so not an exotic case), or a self-introduction ("my
/// name is John"). Each alternative's span covers only the captured name,
/// not the trigger phrase.
///
/// This is the broadest detector in this module by design — see the module
/// docs on why recall matters more than precision here. A signal like
/// "Best," followed by a capitalized phrase is common enough prose that
/// this will occasionally tokenize something that is not a name; that
/// costs a little readability in a preview, not a privacy leak. The
/// sign-off branch's `(?:,\s*|,?\s*[\r\n]+\s*)` connector keeps that
/// tradeoff bounded, though: a comma makes the same line acceptable
/// ("Thanks, Jane"), but with no comma at all a newline is still required
/// — "Best wishes" or "Best New York pizza" must not fire just because a
/// capitalized word happens to follow "Best" on the same line with a
/// space.
///
/// `display_quoted`'s class is `[A-Za-z\ .,'-]`, not `[A-Za-z .,'-]` — see
/// [`OTP`]'s docs for why an unescaped space inside a class silently drops
/// out under `(?x)`. Without the escape, a quoted name with a space in it
/// ("Doe, Jane") does not match at all: the class stops at the comma, the
/// mandatory trailing `[a-zA-Z'-]` has nothing left to consume past it, and
/// the whole alternative fails — quietly, not as a compile error.
static NAME: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(
        r#"(?x)
        (?: (?:Dear|Hi|Hello|Hey)\s+
            (?P<greet>[A-Z][a-zA-Z'-]+(?:\s[A-Z][a-zA-Z'-]+){0,2})\s*[,:]
          | (?:Regards|Best\ regards|Kind\ regards|Warm\ regards|Best|Sincerely|Thanks|
             Thank\ you|Cheers)
            (?:,\s*|,?\s*[\r\n]+\s*)
            (?P<signoff>[A-Z][a-zA-Z'-]+(?:\s[A-Z][a-zA-Z'-]+){0,2})\b
          | "(?P<display_quoted>[A-Z][A-Za-z\ .,'-]{0,78}[a-zA-Z'-])"\s*<[^<>@\s]+@[^<>\s]+>
          | (?P<display>[A-Z][a-zA-Z.'-]+(?:\s[A-Z][a-zA-Z.'-]+){0,2})\s*<[^<>@\s]+@[^<>\s]+>
          | (?i:my\ name\ is|this\ is|i\ am|i'm)\s+
            (?P<intro>[A-Z][a-zA-Z'-]+(?:\s[A-Z][a-zA-Z'-]+){0,2})\b
        )"#,
    )
});

fn scan_names(text: &str) -> Vec<Hit> {
    let Some(re) = NAME.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(text)
        .filter_map(|caps| {
            let name = caps
                .name("greet")
                .or_else(|| caps.name("signoff"))
                .or_else(|| caps.name("display_quoted"))
                .or_else(|| caps.name("display"))
                .or_else(|| caps.name("intro"))?;
            let key = name
                .as_str()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();
            Some(Hit {
                kind: RedactionKind::Name,
                start: name.start(),
                end: name.end(),
                value: name.as_str().to_owned(),
                key,
            })
        })
        .collect()
}

/// Compile a pattern, or record why it could not be compiled.
///
/// Every pattern in this module is a literal, so a failure is a typo caught
/// by `every_pattern_compiles` long before a mailbox sees it — the same
/// discipline `index::entities::compile` applies, and for the same reason:
/// an extractor that silently returns nothing is a firewall category that
/// silently stopped protecting anything, and this is what makes that loud
/// in tests instead of quiet in production.
fn compile(pattern: &str) -> Option<Regex> {
    match Regex::new(pattern) {
        Ok(re) => Some(re),
        Err(error) => {
            tracing::error!(%error, "redaction pattern failed to compile; that detector is disabled");
            None
        }
    }
}

#[cfg(test)]
mod tests;
