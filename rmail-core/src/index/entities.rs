//! Deterministic entity extraction: the things in a message that are worth
//! finding by their shape rather than their words.
//!
//! An invoice number, a tracking code, an IBAN, a sum of money — these are what
//! people actually search for, and none of them survives a lexical index
//! intact. `INV-2024-0231` tokenizes into fragments; `£1,299.00` and
//! `1299 GBP` are the same amount and share no token at all. Recognizing them
//! by pattern turns "the invoice for about twelve hundred pounds" into a query
//! that can succeed.
//!
//! # Normalized identity, original text
//!
//! Every entity carries two forms. `value` is what was written, for display.
//! `norm` is the canonical form, and it is what `UNIQUE(kind, norm)` keys on —
//! so `Ada@Example.COM` and `ada@example.com` are one address, and
//! `+1 (555) 010-1234` and `+15550101234` are one phone. Without that, the
//! entity table becomes a list of spellings rather than a list of things.
//!
//! # Precision over recall, on purpose
//!
//! These extractors are deterministic and they run over every message ever
//! synced. A pattern that is slightly too eager produces thousands of false
//! entities, each of which pollutes the graph, the co-occurrence weights and
//! every search that touches them — and unlike a missed entity, a wrong one is
//! actively misleading. So the patterns are anchored, bounded, and checked
//! (IBANs by their mod-97 checksum, tracking numbers by carrier shape). What
//! they cannot verify, they decline to claim.
//!
//! # Re-extraction replaces
//!
//! A message's mentions are deleted and rewritten on every run. Entities
//! themselves are never deleted here — another message may still refer to one —
//! but a mention that no longer exists must not survive, or a body that lost a
//! phone number would go on being findable by it forever.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::json;

use crate::error::Error;
use crate::storage::Database;

/// Recorded in `entity_mentions.source`, so a regex hit can be told from a
/// model's once there is one.
pub const SOURCE: &str = "regex@1";

/// The longest part this stage will scan.
///
/// Every pattern here is linear, but a pathological body still costs time
/// proportional to its length times the number of extractors. A megabyte of
/// text is far past the point where more entities are useful.
const MAX_SCAN_BYTES: usize = 1024 * 1024;

/// How many distinct entities one message may contribute to the graph.
///
/// Edge writing is quadratic in this number, inside the single writer
/// connection every other write in the process contends on. A link-heavy
/// newsletter with three thousand distinct URLs would otherwise produce four
/// and a half million edge rows and hold that lock for around two minutes.
/// Sixty-four caps it at 2016 pairs, and a message with more distinct entities
/// than that is a directory, not a conversation.
const MAX_ENTITIES_PER_MESSAGE: usize = 64;

/// How many mentions of one entity in one part are worth keeping.
///
/// A mailing-list footer repeating an address forty times is one fact, not
/// forty. The cap bounds the write and keeps the co-occurrence weights from
/// being dominated by boilerplate.
const MAX_MENTIONS_PER_PART: usize = 8;

/// Total text one message may cost the scanner, across all its parts.
///
/// [`MAX_SCAN_BYTES`] bounds a single part, which bounds nothing on its own —
/// a message may carry any number of parts. Four mebibytes of extracted text is
/// far past the point where more entities help.
const MAX_MESSAGE_SCAN_BYTES: usize = 2 * 1024 * 1024;

/// How many distinct discarded entities to remember for the report.
///
/// Purely for the count in [`EntityReport::truncated`]. The set exists to
/// deduplicate — a truncated newsletter names the same dropped URL many times —
/// and a set that grows with the input reintroduces the unbounded allocation
/// the cap is there to prevent.
const MAX_TRUNCATION_TRACKED: usize = 1024;

/// What kind of thing was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntityKind {
    /// An email address.
    Email,
    /// A telephone number.
    Phone,
    /// A URL.
    Url,
    /// A sum of money.
    Amount,
    /// A calendar date.
    Date,
    /// A parcel tracking number.
    TrackingNo,
    /// An order reference.
    OrderId,
    /// An invoice reference.
    InvoiceId,
    /// An international bank account number.
    Iban,
}

impl EntityKind {
    /// Every kind this stage produces.
    pub const ALL: [Self; 9] = [
        Self::Email,
        Self::Phone,
        Self::Url,
        Self::Amount,
        Self::Date,
        Self::TrackingNo,
        Self::OrderId,
        Self::InvoiceId,
        Self::Iban,
    ];

    /// The stable string stored in `entities.kind`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::Phone => "phone",
            Self::Url => "url",
            Self::Amount => "amount",
            Self::Date => "date",
            Self::TrackingNo => "tracking_no",
            Self::OrderId => "order_id",
            Self::InvoiceId => "invoice_id",
            Self::Iban => "iban",
        }
    }

    /// Parse a stored kind.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a kind no version of this code wrote.
    pub fn parse(value: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| Error::internal(format!("unknown entity kind: {value}")))
    }
}

/// One thing found in one place.
#[derive(Debug, Clone, PartialEq)]
pub struct Mention {
    /// What kind of thing.
    pub kind: EntityKind,
    /// As written.
    pub value: String,
    /// Canonical form — the identity.
    pub norm: String,
    /// Kind-specific detail, JSON-encoded.
    pub meta: Option<String>,
    /// Byte offset of the match in the part's normalized text.
    pub span_start: usize,
    /// Byte offset just past the match.
    pub span_end: usize,
    /// How sure the extractor is. A checksummed IBAN is certain; a bare
    /// seven-digit "order number" is a guess.
    pub confidence: f64,
}

/// What one extraction run did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EntityReport {
    /// The message scanned.
    pub message_id: i64,
    /// Distinct entities now linked to it.
    pub entities: usize,
    /// Mentions recorded.
    pub mentions: usize,
    /// Co-occurrence edges created or reinforced.
    pub edges: usize,
    /// Parts skipped because they were empty or too large to be worth scanning.
    pub skipped_parts: usize,
    /// Distinct entities dropped because the message exceeded
    /// [`MAX_ENTITIES_PER_MESSAGE`].
    pub truncated: usize,
}

/// Extract entities from a message's stored parts and record them.
///
/// Replaces the message's mentions wholesale: a body that lost a phone number
/// must stop being findable by it.
///
/// # Errors
///
/// - [`Error::FailedPrecondition`] if the message has no extracted content at
///   all — the extraction stage must run first, and silently recording nothing
///   would look like a message with no entities. Not `NotFound`: the message
///   exists, so saying it is missing sends a client after the wrong problem.
/// - A mapped storage error.
#[tracing::instrument(skip(db), fields(entities, mentions))]
pub async fn extract_entities(db: &Database, message_id: i64) -> Result<EntityReport, Error> {
    let parts: Vec<(String, String)> = db
        .read(move |conn| {
            // Ordered, and ordered by *worth*: the entity cap below drops
            // whatever arrives after the sixty-fourth, so an alphabetical scan
            // would spend the budget on `attachment:` and `body` and leave
            // nothing for `subject` — where the single most searchable thing in
            // a message usually is. Without an explicit order the sequence is
            // an accident of the query plan, and a plan change would churn the
            // entire entity set of every truncated message.
            let mut stmt = conn.prepare(
                "SELECT part, text FROM index_content WHERE message_id = ?1
                 ORDER BY CASE
                     WHEN part = 'subject' THEN 0
                     WHEN part = 'sender' THEN 1
                     WHEN part = 'recipients' THEN 2
                     WHEN part = 'body' THEN 3
                     WHEN part = 'note' THEN 4
                     WHEN part = 'summary' THEN 5
                     ELSE 6
                 END, part",
            )?;
            let rows = stmt
                .query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;

    if parts.is_empty() {
        // Not `NotFound`: the message exists, the pipeline is simply not far
        // enough along. Telling a client the message is missing sends it
        // looking for the wrong problem.
        return Err(Error::failed_precondition(format!(
            "message {message_id} has no extracted content; run the extract stage first"
        )));
    }

    // Scanning is CPU-bound and proportional to body length; it must not hold
    // the runtime or the writer lock.
    let (found, skipped_parts, truncated) = tokio::task::spawn_blocking(move || scan_parts(&parts))
        .await
        .map_err(|e| Error::internal(format!("entity scan task failed: {e}")))?;

    let report = persist(db, message_id, found, skipped_parts, truncated).await?;
    let span = tracing::Span::current();
    span.record("entities", report.entities);
    span.record("mentions", report.mentions);
    tracing::debug!(
        entities = report.entities,
        mentions = report.mentions,
        edges = report.edges,
        "entities extracted"
    );
    Ok(report)
}

/// Scan every part, returning `(part key, mention)` pairs, a skip count and the
/// number of distinct entities the caps discarded.
///
/// The caps are applied *here*, not in `persist`. Enforcing them downstream
/// meant every mention of every entity was materialized first: thirty
/// attachment parts of a megabyte each — an ordinary mail size limit, not an
/// attack — produced 2.17 million `(String, Mention)` pairs, 707 MB resident
/// and 4.9 seconds inside an uncancellable `spawn_blocking`, to then keep 64
/// entities. The budget has to bind before the memory is spent.
fn scan_parts(parts: &[(String, String)]) -> (Vec<(String, Mention)>, usize, usize) {
    let mut found = Vec::new();
    let mut skipped = 0usize;
    let mut admitted: BTreeSet<(EntityKind, String)> = BTreeSet::new();
    let mut dropped: BTreeSet<(EntityKind, String)> = BTreeSet::new();
    let mut budget = MAX_MESSAGE_SCAN_BYTES;
    for (key, text) in parts {
        // Once the cap is reached no later part can contribute a new entity —
        // only more mentions of ones already known, which the per-part mention
        // cap has already decided are not worth much. Scanning on anyway is
        // pure cost, and it is the cost that matters here: this runs inside an
        // uncancellable `spawn_blocking`, so a dropped deadline does not stop
        // it. Parts are read in order of worth, so what is abandoned is the
        // least valuable text in the message.
        if admitted.len() >= MAX_ENTITIES_PER_MESSAGE {
            skipped += 1;
            continue;
        }
        // A message-wide budget as well as a per-part one. Per-part alone
        // bounds nothing: parts are unbounded in number.
        let Some(remaining) = budget.checked_sub(text.len()) else {
            tracing::warn!(part = %key, "message scan budget exhausted; remaining parts skipped");
            skipped += 1;
            continue;
        };
        budget = remaining;
        // An empty part has nothing to find; an enormous one is past the point
        // where more entities help, and both are cheaper to decline than to
        // scan. Neither is an error — a scanned PDF with no text layer is a
        // normal thing to receive.
        if text.is_empty() || text.len() > MAX_SCAN_BYTES {
            if !text.is_empty() {
                tracing::warn!(part = %key, bytes = text.len(), "part too large to scan");
            }
            skipped += 1;
            continue;
        }
        let mut per_entity: BTreeMap<(EntityKind, String), usize> = BTreeMap::new();
        for mention in scan(text) {
            let entity = (mention.kind, mention.norm.clone());
            if !admitted.contains(&entity) {
                if admitted.len() >= MAX_ENTITIES_PER_MESSAGE {
                    // Bounded, because the whole point of this branch is that
                    // the message has an unbounded number of distinct
                    // entities: an unbounded record of what was dropped is the
                    // same leak wearing a different hat.
                    if dropped.len() < MAX_TRUNCATION_TRACKED {
                        dropped.insert(entity);
                    }
                    continue;
                }
                admitted.insert(entity.clone());
            }
            // A mailing-list footer repeating an address forty times is one
            // fact, not forty.
            let seen = per_entity.entry(entity).or_default();
            if *seen >= MAX_MENTIONS_PER_PART {
                continue;
            }
            *seen += 1;
            found.push((key.clone(), mention));
        }
    }
    if !dropped.is_empty() {
        tracing::warn!(
            dropped = dropped.len(),
            cap = MAX_ENTITIES_PER_MESSAGE,
            "message exceeded the entity cap; the excess is not searchable by entity"
        );
    }
    (found, skipped, dropped.len())
}

/// Run every extractor over one piece of text.
///
/// Overlaps are resolved by preferring the match that started earlier and, on a
/// tie, the longer one: `https://track.example.com/1Z999AA10123456784` is a URL,
/// not a URL and a tracking number, and reporting both would double-count it in
/// the graph.
#[must_use]
pub fn scan(text: &str) -> Vec<Mention> {
    let mut candidates: Vec<Mention> = Vec::new();
    candidates.extend(scan_urls(text));
    candidates.extend(scan_emails(text));
    candidates.extend(scan_ibans(text));
    candidates.extend(scan_amounts(text));
    candidates.extend(scan_dates(text));
    candidates.extend(scan_tracking(text));
    candidates.extend(scan_references(text));
    candidates.extend(scan_phones(text));

    candidates.sort_by(|a, b| {
        a.span_start
            .cmp(&b.span_start)
            .then_with(|| b.span_end.cmp(&a.span_end))
    });

    let mut kept: Vec<Mention> = Vec::with_capacity(candidates.len());
    for mention in candidates {
        let overlaps = kept
            .last()
            .is_some_and(|last| mention.span_start < last.span_end);
        if !overlaps {
            kept.push(mention);
        }
    }
    kept
}

// ---------------------------------------------------------------------------
// Extractors
// ---------------------------------------------------------------------------

/// A URL. Bounded to http(s) because a bare `www.` or a `mailto:` is either
/// ambiguous or already another kind.
static URL: LazyLock<Option<Regex>> = LazyLock::new(|| {
    // Trailing punctuation is excluded from the match: a URL at the end of a
    // sentence should not swallow the full stop.
    // A closing paren *is* admitted here, unlike the other trailing
    // punctuation, because it is legal in a path and only `balanced` below has
    // enough context to tell "(see https://example.com/a)" from
    // "https://example.com/a(b)".
    compile(r"https?://[^\s<>\x22]+[^\s<>\x22.,;:!?\]}]")
});

/// An email address. Deliberately narrower than RFC 5322 — the full grammar
/// admits things no mail client has ever sent and matching it here would find
/// an "address" in every line of code.
static EMAIL: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(r"(?i)\b[a-z0-9._%+\-]+@[a-z0-9.\-]+\.[a-z]{2,63}\b"));

/// An IBAN: two letters, two check digits, then up to 30 alphanumerics.
/// Verified by the mod-97 checksum before it is claimed.
static IBAN: LazyLock<Option<Regex>> = LazyLock::new(|| {
    // Case-insensitive: people paste IBANs lowercase, and the checksum below
    // decides whether it is really one.
    compile(r"(?i)\b[A-Z]{2}[0-9]{2}(?:[ ]?[A-Z0-9]{4}){2,7}(?:[ ]?[A-Z0-9]{1,3})?\b")
});

/// A sum of money: a currency symbol or ISO code on either side of a number.
/// A bare number is never an amount — that is the single largest source of
/// false positives in a mailbox full of order numbers and dates.
/// A written number: grouped in threes, or plain, with at most two decimals.
///
/// Ordered longest-first because this crate is leftmost-*first*: with the plain
/// alternative ahead of the grouped one, `1,299.00` would match only `1`.
const NUMBER_PATTERN: &str =
    r"(?:[0-9]{1,3}(?:[.,][0-9]{3})+(?:[.,][0-9]{1,2})?|[0-9]+(?:[.,][0-9]{1,2})?)";

static AMOUNT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    // No whitespace inside the digits. Allowing it lets `$50\n\n42 items` read
    // as fifty thousand and forty-two, and the resulting span reaches across
    // into whatever followed — which the overlap resolver then drops, so one
    // greedy amount both invents a wrong number and loses a real date.
    //
    // Both separators are admitted because both conventions occur: `1,299.00`
    // and `1.299,00` are the same amount, and which is the decimal is decided
    // below rather than assumed here.
    //
    // The digits are a *grammar*, not a character class. `[0-9][0-9.,]*` in the
    // suffix branches walks backwards out of whatever preceded the symbol, so
    // "INV-2024-0231, €1.299,00 due" matched `0231, €` — inventing EUR 231.00
    // and, because that match starts earlier, causing the overlap resolver to
    // discard the real €1.299,00. Groups of three and at most two decimals is
    // what a written amount actually looks like, and it cannot reach across a
    // separator that is not part of a number.
    compile(&format!(
        r"(?ix)
        (?: (?P<sym>[$£€¥])\ ?(?P<n1>{NUMBER_PATTERN})
          | \b(?P<pcode>USD|EUR|GBP|JPY|CHF|CAD|AUD)\ ?(?P<n2>{NUMBER_PATTERN})
          | (?:^|[^0-9.,])(?P<n3>{NUMBER_PATTERN})\ ?(?P<scode>USD|EUR|GBP|JPY|CHF|CAD|AUD)\b
          | (?:^|[^0-9.,])(?P<n4>{NUMBER_PATTERN})\ ?(?P<ssym>[$£€¥])
        )"
    ))
});

/// An ISO-8601 date, or a written one. Slash-separated dates are deliberately
/// absent: `03/04/2024` is March or April depending on which side of the
/// Atlantic wrote it, and a date entity that is wrong half the time is worse
/// than none.
/// Month names, long form first so leftmost-first prefers the complete word.
const MONTH_PATTERN: &str = r"(?:January|February|March|April|May|June|July|August|September|October|November|December|Jan|Feb|Mar|Apr|Jun|Jul|Aug|Sept|Sep|Oct|Nov|Dec)";

static DATE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    // The month must be a *whole word*. A trailing `[a-z]*` turns `Mar` into a
    // prefix and makes "Maroon 5 2024" a date — the same class of mistake as
    // an unanchored `inv`.
    compile(&format!(
        r"(?ix)
        \b(?: (?P<iso>[0-9]{{4}}-[0-9]{{2}}-[0-9]{{2}})(?:T[0-9]{{2}}:[0-9]{{2}}(?::[0-9]{{2}})?(?:Z|[+\-][0-9]{{2}}:?[0-9]{{2}})?)?
            | (?P<dmy>[0-9]{{1,2}}\ {MONTH_PATTERN}\b,?\ [0-9]{{4}})
            | (?P<mdy>{MONTH_PATTERN}\b\ [0-9]{{1,2}},?\ [0-9]{{4}})
        )"
    ))
});

/// Carrier-shaped tracking numbers. Each alternative is a specific carrier's
/// format; a generic "long alphanumeric run" would match half the message ids
/// in a mailbox.
static TRACKING: LazyLock<Option<Regex>> = LazyLock::new(|| {
    // Only UPS and USPS have shapes distinctive enough to claim unaided: `1Z`
    // plus sixteen alphanumerics, and a 22-digit run starting with 9, occur in
    // essentially nothing else. A bare ten- or twelve-digit run does not — it is
    // as likely a unix timestamp, an account number or an unseparated phone —
    // so those carriers must be anchored on a nearby word, exactly as an order
    // reference is.
    compile(
        r"(?ix)
        (?: \b(?P<ups>1Z[0-9A-Z]{16})\b
          | \b(?P<usps>9[0-9]{21})\b
          | (?:track(?:ing)?|shipment|consignment|awb|waybill)
            \s*(?:number|no\.?|\#|:)?\s*
            \b(?P<generic>[0-9]{10,15})\b
        )",
    )
});

/// Order and invoice references. Anchored on the *word*, not the shape: a bare
/// `2024-0231` is a date fragment, a version number, or nothing. Requiring the
/// label is what keeps this from firing on every number in a mailbox.
///
/// The label alone is not enough, though — see [`identifier_shaped`]. The
/// separator group is optional, so on backtracking it matches empty and the
/// next ordinary word becomes the "identifier": "Please find the invoice
/// attached" produced `ATTACHED`, and "Order Number: pending" produced
/// `NUMBER`. Words like that recur in thousands of messages, so each becomes a
/// hub in the co-occurrence graph adjacent to nearly every real entity — and a
/// graph where one node touches everything ranks nothing.
static REFERENCE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    // The trailing `\b` on the label is load-bearing. Without it `inv` matches
    // the prefix of `invoices`, `inventory`, `involved`, `invited` — and the
    // *remainder of the word* becomes the identifier, so ordinary business mail
    // fills the graph with entities like `OICES` and `ENTORY`. Longest
    // alternatives come first because this crate is leftmost-first: with `ref`
    // ahead of `reference`, the word `reference` would never work as a label.
    compile(
        r"(?ix)
        \b(?P<label>invoice|reference|receipt|order|inv|ref)\b
        \s*(?:number|no\.?)?\s*\#?\s*:?\s*
        (?P<id>[A-Z0-9][A-Z0-9\-_/]{2,31})\b",
    )
});

/// A telephone number: international form, or a grouped national one. Requires
/// either a leading `+` or separators, because a bare run of ten digits is far
/// more often an order number than a phone.
static PHONE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    // Bounded at both ends. Unbounded, the greedy international branch reaches
    // across a space into the next number ("+1 555-010-1234 24 hours" becomes a
    // fourteen-digit phone), and the national branch can start mid-run inside a
    // longer identifier.
    // A fixed group shape rather than an open-ended run. An unbounded
    // `[0-9\-. ()]{7,20}` is greedy across a space, so "+1 555-010-1234 24
    // hours" becomes a fourteen-digit number that swallows the "24" — and a
    // digit-count check afterwards cannot catch it, because the result is still
    // a plausible length. Three or four groups covers +1 (555) 010-1234,
    // +44 20 7123 4567 and the rest, and stops where a phone number stops.
    compile(
        r"(?x)
        (?: (?:^|[^0-9+])
            (?P<intl>\+[0-9]{1,3}[\-.\ ]?\(?[0-9]{2,4}\)?[\-.\ ]?[0-9]{2,4}[\-.\ ]?[0-9]{2,6})
          | (?:^|[^0-9\-.])
            (?P<natl>\(?[0-9]{3}\)?[\-.\ ][0-9]{3}[\-.\ ][0-9]{4})(?:$|[^0-9\-.])
        )",
    )
});

/// Compile a pattern, or record why it could not be compiled.
///
/// Every pattern in this module is a literal, so a failure is a typo caught by
/// `every_pattern_compiles` long before a mailbox sees it. It is still not
/// allowed to panic: an extractor that finds nothing degrades search, and
/// taking the daemon down over a regex takes mail down with it.
fn compile(pattern: &str) -> Option<Regex> {
    match Regex::new(pattern) {
        Ok(re) => Some(re),
        Err(error) => {
            tracing::error!(%error, "entity pattern failed to compile; that extractor is disabled");
            None
        }
    }
}

fn scan_urls(text: &str) -> Vec<Mention> {
    let Some(re) = URL.as_ref() else {
        return Vec::new();
    };
    re.find_iter(text)
        .map(|m| {
            // A URL written inside brackets — "(see https://example.com/a)" —
            // keeps the opening paren because it is a legal path character and
            // drops the closing one as sentence punctuation, leaving a URL that
            // resolves to nothing.
            let text = balanced(m.as_str());
            // Trailing slash and case of the host are not identity; the path is.
            let norm = text.trim_end_matches('/').to_owned();
            mention(
                EntityKind::Url,
                text,
                &norm,
                None,
                m.start()..m.start() + text.len(),
                1.0,
            )
        })
        .collect()
}

/// Trim the parentheses a URL cannot account for.
///
/// Parentheses are legal in a path, so the pattern admits them at both ends —
/// but a URL in prose is far more often *inside* a bracket than containing one.
/// Only the balance can tell the two apart: `(see https://example.com/a)` ends
/// with somebody else's bracket, and `https://example.com/a(b)` ends with its
/// own. Trimming either unconditionally breaks one of the two.
fn balanced(url: &str) -> &str {
    let mut end = url.len();
    loop {
        let candidate = url.get(..end).unwrap_or(url);
        let opens = candidate.matches('(').count();
        let closes = candidate.matches(')').count();
        if closes > opens && candidate.ends_with(')') {
            end -= 1;
            continue;
        }
        if opens > closes {
            match candidate.rfind('(') {
                Some(at) => end = at,
                None => return candidate,
            }
            continue;
        }
        return candidate;
    }
}

fn scan_emails(text: &str) -> Vec<Mention> {
    let Some(re) = EMAIL.as_ref() else {
        return Vec::new();
    };
    re.find_iter(text)
        .map(|m| {
            let norm = m.as_str().to_lowercase();
            mention(EntityKind::Email, m.as_str(), &norm, None, m.range(), 1.0)
        })
        .collect()
}

fn scan_ibans(text: &str) -> Vec<Mention> {
    let Some(re) = IBAN.as_ref() else {
        return Vec::new();
    };
    re.find_iter(text)
        .filter_map(|m| {
            // Upper-cased, not merely stripped: the pattern is
            // case-insensitive because people paste IBANs lowercase, and both
            // the checksum and the canonical form are defined over capitals.
            // Leaving the case alone makes a lowercase paste fail validation
            // and, worse, would file `gb82…` and `GB82…` as two accounts.
            let compact: String = m
                .as_str()
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|c| c.to_ascii_uppercase())
                .collect();
            // The checksum is the whole point: without it this pattern matches
            // any capitalized alphanumeric run, and a mailbox is full of those.
            iban_valid(&compact).then(|| {
                mention(
                    EntityKind::Iban,
                    m.as_str(),
                    &compact,
                    Some(json!({ "country": &compact[..2] }).to_string()),
                    m.range(),
                    1.0,
                )
            })
        })
        .collect()
}

/// IBAN mod-97: move the first four characters to the end, map letters to
/// numbers, and the whole thing read as an integer must be 1 modulo 97.
fn iban_valid(compact: &str) -> bool {
    if compact.len() < 15 || compact.len() > 34 {
        return false;
    }
    if !compact[..2].chars().all(|c| c.is_ascii_uppercase())
        || !compact[2..4].chars().all(|c| c.is_ascii_digit())
    {
        return false;
    }
    let rearranged = format!("{}{}", &compact[4..], &compact[..4]);
    let mut remainder: u32 = 0;
    for ch in rearranged.chars() {
        let value = if ch.is_ascii_digit() {
            u32::from(ch as u8 - b'0')
        } else if ch.is_ascii_uppercase() {
            u32::from(ch as u8 - b'A') + 10
        } else {
            return false;
        };
        // Fold two digits at a time for a letter, one for a digit, keeping the
        // running value far below any overflow.
        remainder = if value > 9 {
            (remainder * 100 + value) % 97
        } else {
            (remainder * 10 + value) % 97
        };
    }
    remainder == 1
}

fn scan_amounts(text: &str) -> Vec<Mention> {
    let Some(re) = AMOUNT.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(text)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            // Both orders and both notations occur, often in the same thread:
            // `$42.00`, `EUR 1.234,56`, `42.00 USD`, `42,00 €`.
            // `leads` distinguishes the two shapes for the span below: a
            // prefix marker is part of the amount and starts it, whereas the
            // suffix branches deliberately match one character *before* the
            // number and that character is not part of anything.
            let (digits, currency, leads) = if let Some(sym) = caps.name("sym") {
                (caps.name("n1")?, symbol_currency(sym.as_str()), true)
            } else if let Some(code) = caps.name("pcode") {
                (caps.name("n2")?, code.as_str().to_uppercase(), true)
            } else if let Some(code) = caps.name("scode") {
                (caps.name("n3")?, code.as_str().to_uppercase(), false)
            } else {
                let sym = caps.name("ssym")?;
                (caps.name("n4")?, symbol_currency(sym.as_str()), false)
            };
            let minor = parse_minor_units(digits.as_str())?;
            // Normalized as currency plus integer minor units rendered with a
            // fixed scale. Integers rather than a float because two amounts
            // that differ by a penny must not collide on one `norm`, and a very
            // long digit run must not become `inf` and land un-parseable JSON
            // in `meta`.
            let norm = format!("{currency} {}.{:02}", minor / 100, minor % 100);
            // The suffix branches match the character *before* the number to
            // stop them reaching backwards, and the regex may take a space
            // between number and code. Neither belongs in the span: it should
            // underline the amount and nothing around it.
            let start = if leads { whole.start() } else { digits.start() };
            let span = trim_span(text, start..whole.end());
            Some(mention(
                EntityKind::Amount,
                text.get(span.clone())?,
                &norm,
                Some(
                    json!({
                        "currency": currency,
                        // `u128` is not a JSON number and `as u64` would wrap
                        // a very large sum into a small one. Nothing in a
                        // mailbox needs more than `i128`, and serde_json
                        // represents that natively.
                        "minor_units": i128::try_from(minor).unwrap_or(i128::MAX),
                    })
                    .to_string(),
                ),
                span,
                1.0,
            ))
        })
        .collect()
}

/// Parse a written number into integer minor units, honoring either separator
/// convention.
///
/// `1,299.00` and `1.299,00` are the same amount. Which separator is the
/// decimal cannot be assumed — it is decided by position: the *last* separator
/// is the decimal point if it is followed by one or two digits, and a grouping
/// mark otherwise. `1.299` is one thousand two hundred and ninety-nine, because
/// three trailing digits is a group, not a fraction.
fn parse_minor_units(raw: &str) -> Option<u128> {
    // A separator with no digits after it belongs to the sentence, not the
    // number: `£42.00,` ends a clause, and treating that comma as part of the
    // figure makes it four thousand two hundred.
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect::<String>()
        .trim_end_matches(['.', ','])
        .to_owned();
    let last_sep = cleaned.rfind([',', '.']);
    let (whole, fraction) = match last_sep {
        Some(at) => {
            let tail = &cleaned[at + 1..];
            if tail.len() == 1 || tail.len() == 2 {
                (&cleaned[..at], tail)
            } else {
                (&cleaned[..], "")
            }
        }
        None => (&cleaned[..], ""),
    };
    let whole_digits: String = whole.chars().filter(char::is_ascii_digit).collect();
    if whole_digits.is_empty() && fraction.is_empty() {
        return None;
    }
    // Absurdly long runs are not amounts; refusing them keeps the arithmetic in
    // range and keeps a page of digits out of the entity table.
    if whole_digits.len() > 18 {
        return None;
    }
    let whole_value: u128 = whole_digits.parse().unwrap_or(0);
    let minor: u128 = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u128>().ok()? * 10,
        _ => fraction.parse::<u128>().ok()?,
    };
    Some(whole_value * 100 + minor)
}

/// Narrow a match range to exclude leading whitespace and trailing whitespace
/// or punctuation.
///
/// The highlight should underline the amount, not the space or the comma after
/// it, and the stored display value should not carry them either.
fn trim_span(text: &str, range: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let slice = text.get(range.clone()).unwrap_or_default();
    let start = range.start + (slice.len() - slice.trim_start().len());
    let trimmed = slice.trim_end_matches([' ', '\t', '.', ',']);
    let end = range.start + trimmed.len();
    start..end.max(start)
}

fn symbol_currency(symbol: &str) -> String {
    match symbol {
        "£" => "GBP",
        "€" => "EUR",
        "¥" => "JPY",
        // `$` is ambiguous across a dozen currencies. USD is the majority case
        // and the meta records that it was inferred from a symbol, so a later
        // pass can revisit it with the message's locale.
        _ => "USD",
    }
    .to_owned()
}

fn scan_dates(text: &str) -> Vec<Mention> {
    let Some(re) = DATE.as_ref() else {
        return Vec::new();
    };
    re.find_iter(text)
        .filter_map(|m| {
            let norm = normalize_date(m.as_str())?;
            Some(mention(
                EntityKind::Date,
                m.as_str(),
                &norm,
                None,
                m.range(),
                1.0,
            ))
        })
        .collect()
}

/// Whether a year/month/day triple is a date somebody could have meant.
///
/// `9999-99-99` matches the ISO shape and is not a date; a part number like
/// `1234-56-78` matches it too. Passing either through would put a fiction in
/// the entity graph that no search could ever usefully find.
fn plausible(year: u32, month: u32, day: u32) -> bool {
    const LENGTHS: [u32; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    (1900..=2200).contains(&year)
        && (1..=12).contains(&month)
        && day >= 1
        && usize::try_from(month - 1)
            .ok()
            .and_then(|i| LENGTHS.get(i))
            .is_some_and(|max| day <= *max)
}

/// Reduce a written date to ISO form so `1 Mar 2024` and `2024-03-01` are one
/// entity.
fn normalize_date(text: &str) -> Option<String> {
    let cleaned = text.replace(',', "");
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    let month_of = |token: &str| -> Option<u32> {
        const MONTHS: [&str; 12] = [
            "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
        ];
        let lower = token.to_lowercase();
        // The regex already required a whole month word, so a three-letter
        // prefix match here is safe rather than the loophole it was.
        MONTHS
            .iter()
            .position(|m| lower.starts_with(m))
            .and_then(|i| u32::try_from(i).ok())
            .map(|i| i + 1)
    };
    let (year, month, day) = match tokens.as_slice() {
        // Already ISO, possibly with a time. The entity is the *day*: a
        // deadline and the calendar invite that names it are the same date, and
        // filing them apart would defeat the point of normalizing at all.
        [only] if only.len() >= 10 && only.as_bytes().get(4) == Some(&b'-') => (
            only[..4].parse().ok()?,
            only[5..7].parse().ok()?,
            only[8..10].parse().ok()?,
        ),
        // `1 Mar 2024`
        [day, month, year] if day.chars().all(|c| c.is_ascii_digit()) => {
            (year.parse().ok()?, month_of(month)?, day.parse().ok()?)
        }
        // `Mar 1 2024`
        [month, day, year] => (year.parse().ok()?, month_of(month)?, day.parse().ok()?),
        _ => return None,
    };
    plausible(year, month, day).then(|| format!("{year:04}-{month:02}-{day:02}"))
}

fn scan_tracking(text: &str) -> Vec<Mention> {
    let Some(re) = TRACKING.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(text)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            // The shaped carriers identify themselves; the anchored branch only
            // knows that a nearby word claimed it was a tracking number, which
            // is a weaker thing to know and says so in its confidence.
            let (carrier, code, confidence) = if let Some(m) = caps.name("ups") {
                ("ups", m, 0.95)
            } else if let Some(m) = caps.name("usps") {
                ("usps", m, 0.95)
            } else {
                ("unknown", caps.name("generic")?, 0.7)
            };
            let _ = whole;
            Some(mention(
                EntityKind::TrackingNo,
                code.as_str(),
                &code.as_str().to_uppercase(),
                Some(json!({ "carrier": carrier }).to_string()),
                code.range(),
                confidence,
            ))
        })
        .collect()
}

fn scan_references(text: &str) -> Vec<Mention> {
    let Some(re) = REFERENCE.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(text)
        .filter_map(|caps| {
            let id = caps.name("id")?;
            if !identifier_shaped(id.as_str()) {
                return None;
            }
            let label = caps.name("label")?.as_str().to_lowercase();
            let kind = if label.starts_with("inv") {
                EntityKind::InvoiceId
            } else if label.starts_with("order") {
                EntityKind::OrderId
            } else {
                // `ref`/`receipt`/`reference` are ambiguous between the two;
                // an order is the commoner reading in a mailbox.
                EntityKind::OrderId
            };
            // The span covers the identifier alone, not the label, so
            // highlighting marks the thing rather than the sentence.
            Some(mention(
                kind,
                id.as_str(),
                &id.as_str().to_uppercase(),
                None,
                id.range(),
                0.8,
            ))
        })
        .collect()
}

/// Whether a candidate reference is shaped like an identifier rather than a
/// word.
///
/// A digit is the discriminator. Every real order, invoice and receipt number
/// carries one; `ATTACHED`, `CONFIRMATION`, `NUMBER`, `DATE` and `TOTAL` — the
/// words that actually follow these labels in business mail — carry none. An
/// all-digit run is admitted, because `Invoice 100482` is ordinary; a run of
/// letters never is.
fn identifier_shaped(id: &str) -> bool {
    id.chars().any(|c| c.is_ascii_digit())
}

fn scan_phones(text: &str) -> Vec<Mention> {
    let Some(re) = PHONE.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(text)
        .filter_map(|caps| {
            // The boundary characters are matched but not part of the number.
            let m = caps.name("intl").or_else(|| caps.name("natl"))?;
            let digits: String = m.as_str().chars().filter(char::is_ascii_digit).collect();
            // Below seven digits it is a year range or a part number; above
            // fifteen it violates E.164 and is something else entirely.
            if digits.len() < 7 || digits.len() > 15 {
                return None;
            }
            let norm = if m.as_str().trim_start().starts_with('+') {
                format!("+{digits}")
            } else {
                digits.clone()
            };
            let confidence = if norm.starts_with('+') { 0.9 } else { 0.7 };
            Some(mention(
                EntityKind::Phone,
                m.as_str().trim(),
                &norm,
                None,
                m.range(),
                confidence,
            ))
        })
        .collect()
}

fn mention(
    kind: EntityKind,
    value: &str,
    norm: &str,
    meta: Option<String>,
    range: std::ops::Range<usize>,
    confidence: f64,
) -> Mention {
    Mention {
        kind,
        value: value.to_owned(),
        norm: norm.to_owned(),
        meta,
        span_start: range.start,
        span_end: range.end,
        confidence,
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Record what was found, replacing this message's previous mentions.
async fn persist(
    db: &Database,
    message_id: i64,
    found: Vec<(String, Mention)>,
    skipped_parts: usize,
    truncated: usize,
) -> Result<EntityReport, Error> {
    let report = db
        .write(move |conn| {
            let tx = conn.transaction()?;

            // What this message contributed last time. Its pairs must be
            // recomputed alongside the new ones, or an entity that left the
            // body would keep its edge for ever.
            let previous: Vec<i64> = {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT entity_id FROM entity_mentions WHERE message_id = ?1",
                )?;
                let rows = stmt
                    .query_map([message_id], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                rows
            };

            // Replace, not merge: a body that lost a phone number must stop
            // being findable by it. The entities themselves stay — another
            // message may still refer to them.
            tx.execute(
                "DELETE FROM entity_mentions WHERE message_id = ?1",
                [message_id],
            )?;

            let mut ids: BTreeMap<(String, String), i64> = BTreeMap::new();
            let mut mentions = 0usize;
            for (part, m) in &found {
                let kind = m.kind.as_str().to_owned();
                let key = (kind.clone(), m.norm.clone());
                let entity_id = match ids.get(&key) {
                    Some(id) => *id,
                    None => {
                        // `UNIQUE(kind, norm)` makes this an upsert: the
                        // canonical form is the identity, so a second spelling
                        // finds the first rather than creating a twin.
                        tx.prepare_cached(
                            "INSERT INTO entities (kind, value, norm, meta)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(kind, norm) DO UPDATE SET
                                 meta = COALESCE(excluded.meta, entities.meta)",
                        )?
                        .execute(rusqlite::params![kind, m.value, m.norm, m.meta])?;
                        let id: i64 = tx.query_row(
                            "SELECT entity_id FROM entities WHERE kind = ?1 AND norm = ?2",
                            rusqlite::params![kind, m.norm],
                            |row| row.get(0),
                        )?;
                        ids.insert(key, id);
                        id
                    }
                };
                mentions += tx
                    .prepare_cached(
                        "INSERT INTO entity_mentions
                             (entity_id, message_id, part, span_start, span_end, source,
                              confidence)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(entity_id, message_id, part, span_start) DO UPDATE SET
                             span_end = excluded.span_end,
                             source = excluded.source,
                             confidence = excluded.confidence",
                    )?
                    .execute(rusqlite::params![
                        entity_id,
                        message_id,
                        part,
                        i64::try_from(m.span_start).unwrap_or(i64::MAX),
                        i64::try_from(m.span_end).unwrap_or(i64::MAX),
                        SOURCE,
                        m.confidence,
                    ])?;
            }

            let current: BTreeSet<i64> = ids.values().copied().collect();
            // Every pair this message could have changed has both ends in
            // `previous ∪ current`, so recomputing exactly that neighbourhood
            // is both sufficient and bounded.
            let affected: Vec<i64> = current
                .union(&previous.iter().copied().collect())
                .copied()
                .collect();
            let edges = sync_edges(&tx, &affected)?;

            tx.commit()?;
            Ok(EntityReport {
                message_id,
                entities: current.len(),
                mentions,
                edges,
                skipped_parts,
                truncated,
            })
        })
        .await?;
    Ok(report)
}

/// Recompute the co-occurrence weight of every pair among `entities` from the
/// mentions themselves.
///
/// # Derived, not accumulated
///
/// This was a `+1`/`-1` pair of passes, and it was wrong twice over.
///
/// It depended on both the withdrawal set and the contribution set arriving in
/// ascending id order, because the writer emitted `(entities[i], entities[j])`
/// for `i < j` and relied on that being `src_id < dst_id`. The contribution set
/// came from a `BTreeSet` and was sorted; the withdrawal set came from a
/// `SELECT DISTINCT` with no `ORDER BY` and came back in mention order, which
/// is *textual* order. Any reply that mentioned an entity created by an earlier
/// message — that is, most mail in a thread — withdrew from `(hi, lo)` while
/// contributing to `(lo, hi)`. The withdrawal created a phantom row that the
/// zero sweep immediately deleted, and the real weight climbed on every
/// redelivery. The corruption was partial, silent, and left no trace to repair
/// from.
///
/// It also could not survive a deleted message: the mentions cascade away, but
/// nothing revisits the pairs, so the weight of a conversation that no longer
/// exists stays in the ranking for ever.
///
/// Counting `DISTINCT message_id` over the mentions removes both failures by
/// construction rather than by discipline: the weight is a function of the
/// current mention table, so redelivery is free, order is irrelevant, and a
/// cascade is reflected the next time the neighbourhood is touched.
fn sync_edges(conn: &rusqlite::Connection, entities: &[i64]) -> rusqlite::Result<usize> {
    if entities.len() < 2 {
        return Ok(0);
    }
    // An `IN` list built from `i64`s rather than bound parameters: SQLite caps
    // bound parameters far below the number of pairs this can touch, and an
    // integer has no injection surface.
    let list = entities
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");

    // Scoped to the neighbourhood, not the table. The previous zero sweep was
    // `DELETE FROM entity_edges WHERE weight <= 0.0` — a full scan of the edge
    // table inside the single writer connection, on every message indexed.
    conn.execute(
        &format!(
            "DELETE FROM entity_edges
             WHERE rel = 'co_occurs' AND src_id IN ({list}) AND dst_id IN ({list})"
        ),
        [],
    )?;
    let written = conn.execute(
        &format!(
            "INSERT INTO entity_edges (src_id, dst_id, rel, weight)
             SELECT a.entity_id, b.entity_id, 'co_occurs', COUNT(DISTINCT a.message_id)
             FROM entity_mentions a
             JOIN entity_mentions b
               ON b.message_id = a.message_id AND b.entity_id > a.entity_id
             WHERE a.entity_id IN ({list}) AND b.entity_id IN ({list})
             GROUP BY a.entity_id, b.entity_id
             ON CONFLICT(src_id, dst_id, rel) DO UPDATE SET weight = excluded.weight"
        ),
        [],
    )?;
    Ok(written)
}

/// Above this many affected entities, reconcile the whole edge table instead of
/// naming every one of them.
///
/// The targeted path builds an `IN` list and joins it to itself, which is
/// quadratic in the list. Past a few hundred entities a single set-based pass
/// over the table is both cheaper and simpler than a statement the size of a
/// mailbox.
const MAX_TARGETED_RECONCILE: usize = 512;

/// Withdraw some messages' contribution to the entity graph.
///
/// Deleting a row from `messages` cascades its mentions away but leaves the
/// co-occurrence weights they supported, because nothing revisits those pairs.
/// Call this *before* deleting the messages, inside the same transaction, so
/// the graph never describes mail that no longer exists.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn withdraw_messages(conn: &rusqlite::Connection, ids: &[i64]) -> rusqlite::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let list = ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let affected: Vec<i64> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT entity_id FROM entity_mentions WHERE message_id IN ({list})"
        ))?;
        let rows = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        rows
    };
    conn.execute(
        &format!("DELETE FROM entity_mentions WHERE message_id IN ({list})"),
        [],
    )?;
    if affected.len() > MAX_TARGETED_RECONCILE {
        reconcile_edges(conn)?;
    } else {
        sync_edges(conn, &affected)?;
    }
    Ok(())
}

/// Recompute every co-occurrence weight from the mentions.
///
/// The set-based twin of [`sync_edges`], for the cases where naming the
/// affected entities would cost more than reading the table: a `UIDVALIDITY`
/// bump invalidating a six-figure folder, or a repair after the incremental
/// weights were ever allowed to drift.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn reconcile_edges(conn: &rusqlite::Connection) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM entity_edges
         WHERE rel = 'co_occurs' AND NOT EXISTS (
             SELECT 1 FROM entity_mentions a
             JOIN entity_mentions b ON b.message_id = a.message_id
             WHERE a.entity_id = entity_edges.src_id
               AND b.entity_id = entity_edges.dst_id
         )",
        [],
    )?;
    conn.execute(
        "INSERT INTO entity_edges (src_id, dst_id, rel, weight)
         SELECT a.entity_id, b.entity_id, 'co_occurs', COUNT(DISTINCT a.message_id)
         FROM entity_mentions a
         JOIN entity_mentions b
           ON b.message_id = a.message_id AND b.entity_id > a.entity_id
         GROUP BY a.entity_id, b.entity_id
         ON CONFLICT(src_id, dst_id, rel) DO UPDATE SET weight = excluded.weight",
        [],
    )
}

/// Withdraw one message's contribution to the entity graph.
///
/// The asynchronous single-message form of [`withdraw_messages`], for callers
/// outside the sync path that hold a [`Database`] rather than a connection.
///
/// # Errors
///
/// A mapped storage error.
#[tracing::instrument(skip(db))]
pub async fn forget_message(db: &Database, message_id: i64) -> Result<(), Error> {
    db.write(move |conn| {
        let tx = conn.transaction()?;
        withdraw_messages(&tx, &[message_id])?;
        tx.commit()?;
        Ok(())
    })
    .await?;
    Ok(())
}

/// Delete entities nothing mentions any more.
///
/// Entities are not removed when a message is: another message may still refer
/// to one, and the cascade only takes the mentions. But an entity with no
/// mentions left is a fragment of deleted mail — an IBAN, a phone number, a
/// home address — sitting in the database with nothing pointing at it. Mail a
/// user deleted should not leave its contents behind.
///
/// # Errors
///
/// A mapped storage error.
#[tracing::instrument(skip(db))]
pub async fn collect_orphans(db: &Database) -> Result<u64, Error> {
    let mut removed = 0u64;
    loop {
        // Batched, and the writer released between batches. A full-table
        // `DELETE` holds the single writer connection every other write in the
        // process is queued behind, for as long as the sweep takes — which
        // after a large mailbox is deleted is exactly when the rest of the
        // daemon is busiest.
        let batch = db
            .write(|conn| {
                conn.execute(
                    "DELETE FROM entities WHERE entity_id IN (
                         SELECT entity_id FROM entities
                         WHERE entity_id NOT IN (SELECT entity_id FROM entity_mentions)
                         LIMIT ?1
                     )",
                    [ORPHAN_BATCH],
                )
            })
            .await?;
        removed += batch as u64;
        if batch < ORPHAN_BATCH as usize {
            break;
        }
        // Somebody else has been waiting for the writer for a whole batch.
        tokio::task::yield_now().await;
    }
    if removed > 0 {
        tracing::info!(removed, "collected entities with no remaining mentions");
    }
    Ok(removed)
}

/// How many orphans one pass of [`collect_orphans`] removes before releasing
/// the writer.
const ORPHAN_BATCH: i64 = 1024;

#[cfg(test)]
mod tests;
