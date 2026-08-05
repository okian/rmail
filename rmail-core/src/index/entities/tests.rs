//! Fixtures per kind, the normalization that makes two spellings one thing, and
//! the precision guards that keep a mailbox full of numbers from becoming a
//! mailbox full of false entities.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::repo;
use crate::ErrorReason;
use rusqlite::OptionalExtension;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The kinds found in some text, for terse assertions.
fn kinds(text: &str) -> Vec<EntityKind> {
    scan(text).into_iter().map(|m| m.kind).collect()
}

/// The normalized forms found in some text.
fn norms(text: &str) -> Vec<String> {
    scan(text).into_iter().map(|m| m.norm).collect()
}

/// The single mention expected in some text.
fn one(text: &str) -> Mention {
    let found = scan(text);
    assert_eq!(
        found.len(),
        1,
        "expected exactly one entity in {text:?}: {found:?}"
    );
    found.into_iter().next().unwrap_or_else(|| unreachable!())
}

// ---------------------------------------------------------------------------
// Per-kind fixtures
// ---------------------------------------------------------------------------

#[test]
fn emails_are_found_and_case_folded() {
    let m = one("write to Ada@Example.COM about it");
    assert_eq!(m.kind, EntityKind::Email);
    assert_eq!(m.value, "Ada@Example.COM", "as written, for display");
    assert_eq!(m.norm, "ada@example.com", "canonical, for identity");
}

#[test]
fn urls_are_found_without_their_trailing_punctuation() {
    let m = one("see https://example.com/orders/42.");
    assert_eq!(m.kind, EntityKind::Url);
    assert_eq!(
        m.value, "https://example.com/orders/42",
        "the full stop ends the sentence, not the URL"
    );
}

#[test]
fn a_trailing_slash_is_not_identity() {
    assert_eq!(
        norms("https://example.com/docs/"),
        norms("https://example.com/docs")
    );
}

#[test]
fn ibans_are_checksummed_before_they_are_claimed() {
    // Without mod-97 this pattern matches any capitalized alphanumeric run, and
    // a mailbox is full of those.
    let m = one("transfer to GB82 WEST 1234 5698 7654 32 please");
    assert_eq!(m.kind, EntityKind::Iban);
    assert_eq!(m.norm, "GB82WEST12345698765432", "spaces are not identity");
    assert_eq!(m.meta.as_deref(), Some(r#"{"country":"GB"}"#));

    // One digit changed: the checksum fails and nothing is claimed.
    assert!(
        !kinds("transfer to GB82 WEST 1234 5698 7654 33 please").contains(&EntityKind::Iban),
        "a bad checksum is not an IBAN"
    );
}

#[test]
fn amounts_carry_a_currency_and_a_normalized_value() {
    let m = one("the total is £1,299.00 including VAT");
    assert_eq!(m.kind, EntityKind::Amount);
    assert_eq!(m.norm, "GBP 1299.00");

    // The same amount written differently is the same entity — which is the
    // whole point, since the two share no lexical token.
    assert_eq!(norms("£1299"), norms("£1,299.00"));
    assert_eq!(norms("1299.00 GBP"), norms("£1,299.00"));
}

#[test]
fn a_bare_number_is_never_an_amount() {
    // The single largest source of false positives in a mailbox full of order
    // numbers, dates and version strings.
    assert!(!kinds("the figure was 1299 last quarter").contains(&EntityKind::Amount));
    assert!(!kinds("we shipped 42 units").contains(&EntityKind::Amount));
}

#[test]
fn dates_normalize_to_iso_however_they_were_written() {
    for text in ["2024-03-01", "1 Mar 2024", "Mar 1, 2024", "March 1 2024"] {
        let m = one(text);
        assert_eq!(m.kind, EntityKind::Date, "{text}");
        assert_eq!(m.norm, "2024-03-01", "{text} should normalize to ISO");
    }
}

#[test]
fn an_ambiguous_slash_date_is_not_claimed() {
    // 03/04/2024 is March or April depending on which side of the Atlantic
    // wrote it. A date entity that is wrong half the time is worse than none.
    assert!(!kinds("due 03/04/2024").contains(&EntityKind::Date));
}

#[test]
fn tracking_numbers_carry_their_carrier() {
    let m = one("your parcel 1Z999AA10123456784 is out for delivery");
    assert_eq!(m.kind, EntityKind::TrackingNo);
    assert_eq!(m.meta.as_deref(), Some(r#"{"carrier":"ups"}"#));
    assert!(m.confidence > 0.9, "a UPS-shaped code is a strong claim");

    let digits = one("tracking 123456789012 shipped");
    assert_eq!(digits.kind, EntityKind::TrackingNo);
    assert!(
        digits.confidence < 0.9,
        "a bare run of digits is a weak one: {}",
        digits.confidence
    );
}

#[test]
fn order_and_invoice_references_need_their_label() {
    // Anchored on the word, not the shape: a bare `2024-0231` is a date
    // fragment, a version number, or nothing.
    let invoice = one("see invoice INV-2024-0231 attached");
    assert_eq!(invoice.kind, EntityKind::InvoiceId);
    assert_eq!(invoice.norm, "INV-2024-0231");

    let order = one("your order #ABC-99812 has shipped");
    assert_eq!(order.kind, EntityKind::OrderId);

    assert!(
        kinds("the value ABC-99812 appeared").is_empty(),
        "an unlabelled identifier is not a reference"
    );
}

#[test]
fn a_reference_span_covers_the_identifier_not_the_sentence() {
    // Highlighting should mark the thing, not the words around it.
    let text = "see invoice INV-2024-0231 attached";
    let m = one(text);
    assert_eq!(&text[m.span_start..m.span_end], "INV-2024-0231");
}

#[test]
fn phones_need_a_plus_or_separators() {
    // A bare run of ten digits is far more often an order number than a phone.
    let intl = one("call +1 (555) 010-1234 today");
    assert_eq!(intl.kind, EntityKind::Phone);
    assert_eq!(intl.norm, "+15550101234");
    assert!(intl.confidence > 0.8);

    let national = one("call 555-010-1234 today");
    assert_eq!(national.norm, "5550101234");

    assert!(
        scan("the id 5550101234 was logged").is_empty(),
        "an unseparated digit run is not a phone number, nor anything else"
    );
}

#[test]
fn every_kind_has_a_stable_wire_string() {
    // Stored in `entities.kind`; changing one orphans every row that used it.
    assert_eq!(EntityKind::Email.as_str(), "email");
    assert_eq!(EntityKind::Phone.as_str(), "phone");
    assert_eq!(EntityKind::Url.as_str(), "url");
    assert_eq!(EntityKind::Amount.as_str(), "amount");
    assert_eq!(EntityKind::Date.as_str(), "date");
    assert_eq!(EntityKind::TrackingNo.as_str(), "tracking_no");
    assert_eq!(EntityKind::OrderId.as_str(), "order_id");
    assert_eq!(EntityKind::InvoiceId.as_str(), "invoice_id");
    assert_eq!(EntityKind::Iban.as_str(), "iban");
    for kind in EntityKind::ALL {
        assert_eq!(EntityKind::parse(kind.as_str()).unwrap(), kind);
    }
    assert_eq!(
        EntityKind::parse("nope").unwrap_err().reason(),
        ErrorReason::Internal
    );
}

// ---------------------------------------------------------------------------
// Overlap and precision
// ---------------------------------------------------------------------------

#[test]
fn a_tracking_number_inside_a_url_is_only_a_url() {
    // Reporting both would double-count the same text in the graph and in the
    // co-occurrence weights.
    let found = scan("track at https://example.com/t/1Z999AA10123456784 now");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, EntityKind::Url);
}

#[test]
fn an_email_inside_a_url_is_only_a_url() {
    let found = scan("open https://example.com/u/ada@example.com/profile");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, EntityKind::Url);
}

#[test]
fn an_amount_does_not_swallow_what_follows_it() {
    // "$42.00 5 items" must be one amount, not four thousand two hundred and
    // five. A greedy digit class that reaches across a space produces a
    // plausible-looking number, which is exactly why nothing downstream catches
    // it.
    let m = one("total $42.00 5 items");
    assert_eq!(m.kind, EntityKind::Amount);
    assert_eq!(m.norm, "USD 42.00");
}

#[test]
fn continental_and_anglo_number_formats_are_one_amount() {
    // 1.234,56 and 1,234.56 are the same money written by two different
    // conventions; treating them as different entities splits a supplier's
    // invoices in half.
    assert_eq!(one("EUR 1.234,56 due").norm, one("EUR 1,234.56 due").norm);
    assert_eq!(one("EUR 1.234,56 due").norm, "EUR 1234.56");
}

#[test]
fn amounts_that_differ_by_a_penny_do_not_collide() {
    assert_ne!(one("$42.00").norm, one("$42.01").norm);
    assert_ne!(one("$42.00").norm, one("$4200.00").norm);
}

#[test]
fn an_amount_span_covers_the_amount_and_nothing_else() {
    let text = "paid £42.00, thanks";
    let m = one(text);
    assert_eq!(&text[m.span_start..m.span_end], "£42.00");
    assert_eq!(m.norm, "GBP 42.00");
}

#[test]
fn a_tracking_number_without_a_distinctive_shape_needs_a_label() {
    // A UPS 1Z number is unmistakable. A twelve-digit FedEx number is
    // indistinguishable from an account number, an order id or a phone number
    // with the separators stripped, so it is only a tracking number when the
    // text says so.
    assert_eq!(kinds("1Z999AA10123456784"), vec![EntityKind::TrackingNo]);
    assert!(
        scan("your account 123456789012 was debited").is_empty(),
        "twelve bare digits are not evidence of a parcel"
    );
    assert_eq!(
        kinds("FedEx tracking 123456789012"),
        vec![EntityKind::TrackingNo],
        "but with the carrier named, they are"
    );
}

#[test]
fn a_phone_does_not_reach_into_the_next_number() {
    let text = "call +1 555-010-1234 24 hours a day";
    let m = one(text);
    assert_eq!(m.norm, "+15550101234");
    assert_eq!(&text[m.span_start..m.span_end], "+1 555-010-1234");
}

#[test]
fn the_word_reference_works_as_a_label() {
    // Longest-alternative-first ordering: `ref` matching before `reference`
    // leaves "erence" attached to the identifier.
    assert_eq!(one("reference ABC-1234 please").norm, "ABC-1234");
    assert_eq!(one("ref ABC-1234 please").norm, "ABC-1234");
}

#[test]
fn a_lowercase_iban_is_still_an_iban() {
    assert_eq!(
        one("pay to gb82west12345698765432").norm,
        one("pay to GB82WEST12345698765432").norm
    );
}

#[test]
fn spans_survive_multi_byte_text_before_them() {
    // Spans are byte offsets into the same string the caller holds; a character
    // count would land mid-codepoint here and panic on slicing.
    let text = "Grüße — mail ada@example.com";
    let m = one(text);
    assert_eq!(&text[m.span_start..m.span_end], "ada@example.com");
}

#[test]
fn a_timestamp_is_the_day_it_names() {
    // The form calendar invites, `Date:` headers and every machine-written
    // deadline use. A deadline and the invite that names it are the same date;
    // filing them apart defeats the point of normalizing at all.
    for text in [
        "2024-03-01",
        "starts 2024-03-01T09:00:00Z",
        "due 2024-03-01T09:00+01:00",
        "at 2024-03-01T09:00:00",
    ] {
        let m = one(text);
        assert_eq!(m.kind, EntityKind::Date, "{text}");
        assert_eq!(m.norm, "2024-03-01", "{text}");
    }
}

#[test]
fn a_url_in_brackets_keeps_neither_bracket() {
    // A paren is legal in a path, so the pattern admits it — and the closing
    // one is trimmed as sentence punctuation, leaving a URL that resolves to
    // nothing.
    let m = one("see (https://example.com/a) for details");
    assert_eq!(m.value, "https://example.com/a");
    // But a genuinely parenthesised path keeps both.
    assert_eq!(
        one("see https://example.com/a(b) now").value,
        "https://example.com/a(b)"
    );
}

#[test]
fn entity_metadata_is_valid_json() {
    // `meta` is read back by the API layer; a hand-built string is one careless
    // kind away from being unparsable there instead of here.
    for text in [
        "pay GB82WEST12345698765432",
        "parcel 1Z999AA10123456784",
        "total $1,299.00",
    ] {
        for m in scan(text) {
            if let Some(meta) = &m.meta {
                assert!(
                    serde_json::from_str::<serde_json::Value>(meta).is_ok(),
                    "unparsable meta {meta:?} from {text:?}"
                );
            }
        }
    }
}

#[test]
fn every_pattern_compiles() {
    // The patterns are literals, so a failure is a typo — but they are built
    // through `compile`, which declines rather than panicking, and a declining
    // extractor is silent. This is what makes the typo loud.
    for (name, pattern) in [
        ("url", URL.as_ref()),
        ("email", EMAIL.as_ref()),
        ("iban", IBAN.as_ref()),
        ("amount", AMOUNT.as_ref()),
        ("date", DATE.as_ref()),
        ("tracking", TRACKING.as_ref()),
        ("reference", REFERENCE.as_ref()),
        ("phone", PHONE.as_ref()),
    ] {
        assert!(pattern.is_some(), "the {name} pattern did not compile");
    }
}

#[test]
fn a_label_followed_by_an_ordinary_word_is_not_a_reference() {
    // The separator group is optional, so on backtracking it matches empty and
    // the next word becomes the "identifier". These are not exotic strings —
    // they are what business mail says, and `NUMBER`, `DATE` and `TOTAL` recur
    // in thousands of messages, so each would become a graph hub adjacent to
    // nearly every real entity.
    for text in [
        "Please find the invoice attached for your records.",
        "Your order confirmation is below.",
        "Order Number: pending",
        "Order Total: $42.00",
        "Invoice  Date        Amount",
        "invoice queries to billing@acme.io",
        "the order books are open",
        "Ref below for details.",
        "We received your order today, thanks.",
        "your receipt follows shortly",
    ] {
        let kinds = kinds(text);
        assert!(
            !kinds.contains(&EntityKind::OrderId) && !kinds.contains(&EntityKind::InvoiceId),
            "false reference in {text:?}: {:?}",
            scan(text)
        );
    }
}

#[test]
fn a_reference_survives_the_separators_people_actually_type() {
    for (text, expected) in [
        ("Order #: A-1234", "A-1234"),
        ("Invoice#: INV-1", "INV-1"),
        ("invoice no. 100482", "100482"),
        ("Order number 5567-B", "5567-B"),
        ("reference ABC-1234 please", "ABC-1234"),
    ] {
        let found = scan(text);
        assert!(
            found.iter().any(|m| m.norm == expected),
            "expected {expected:?} in {text:?}, got {found:?}"
        );
    }
}

#[test]
fn an_amount_does_not_reach_backwards_across_a_separator() {
    // The suffix branch used to walk backwards out of whatever preceded a
    // *prefix* symbol, so an ordinary invoice line invented a number — and,
    // because the invention started earlier in the text, the overlap resolver
    // then discarded the real amount. Both halves of that are checked here.
    for (text, expected) in [
        ("INV-2024-0231, \u{20ac}1.299,00 due", "EUR 1299.00"),
        ("order 5678, $29.99 total", "USD 29.99"),
        ("item 12345, \u{a3}5.00", "GBP 5.00"),
        ("SKU,QTY,PRICE\nABC-1234,2,$19.99", "USD 19.99"),
        ("1,2,3,$4.00", "USD 4.00"),
    ] {
        let amounts: Vec<String> = scan(text)
            .into_iter()
            .filter(|m| m.kind == EntityKind::Amount)
            .map(|m| m.norm)
            .collect();
        assert_eq!(
            amounts,
            vec![expected.to_owned()],
            "in {text:?}: exactly one amount, and it is the real one"
        );
    }
}

#[test]
fn spans_never_overlap() {
    let text = "Order #A-1234 for £99.99 to ada@example.com by 2024-03-01, \
                tracked as 1Z999AA10123456784, call +1 555-010-1234.";
    let found = scan(text);
    assert_eq!(
        found.len(),
        6,
        "exactly the six planted entities: `>=` also passes when one is lost, \
         which is half of what this fixture is for. found: {found:?}"
    );
    for pair in found.windows(2) {
        assert!(
            pair[0].span_end <= pair[1].span_start,
            "overlapping spans: {:?} and {:?}",
            pair[0],
            pair[1]
        );
    }
    // And every span indexes the text it claims to.
    for m in &found {
        assert_eq!(&text[m.span_start..m.span_end], m.value);
    }
}

#[test]
fn ordinary_prose_yields_nothing() {
    // The precision bar, and the test most worth being adversarial about: an
    // extractor that fires on normal writing pollutes the graph, the
    // co-occurrence weights and every search that touches them — and unlike a
    // missed entity, a wrong one is actively misleading.
    //
    // Every line past the first four is one an earlier draft got wrong.
    for text in [
        "Thanks for the update, I'll take a look this afternoon.",
        "We shipped 42 units in Q3 and expect 60 in Q4.",
        "Version 2.10.4 is out; see the changelog.",
        "Meeting moved to 3pm.",
        // An unanchored `inv` label turns the rest of the word into an id.
        "we have invoices pending review",
        "check the inventory levels today",
        "this is an investment opportunity",
        "everyone involved should reply",
        "you are invited to join us",
        "the invitation is attached",
        "refunds will be issued next week",
        "we referenced your earlier note",
        // A month name matched as a word prefix.
        "Maroon 5 2024 tour dates",
        "Marathon 26 2024 results",
        // ISO-shaped strings that are not dates.
        "9999-99-99 is not a date",
        "part 1234-56-78 shipped",
        // Bare digit runs that are not tracking numbers.
        "unsubscribe id 1712345678 recorded",
        "your account 123456789012 was debited",
        "the id 5550101234 was logged",
    ] {
        assert!(
            scan(text).is_empty(),
            "false positives in {text:?}: {:?}",
            scan(text)
        );
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

struct Fixture {
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-entities-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let (account_id, mailbox_id) = db
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
        Self {
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    /// A message whose `body` part holds `text`, as extraction would have left
    /// it.
    async fn message_with(&self, text: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let text = text.to_owned();
        self.db
            .write(move |c| {
                let id = repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )?;
                c.execute(
                    "INSERT INTO index_content
                         (message_id, part, text, chars, content_hash, extractor)
                     VALUES (?1, 'body', ?2, ?3, X'00', 'test')",
                    rusqlite::params![id, text, text.len() as i64],
                )?;
                Ok(id)
            })
            .await
            .unwrap()
    }

    fn entity_count(&self) -> i64 {
        self.db
            .with_read(|c| c.query_row("SELECT count(*) FROM entities", [], |r| r.get(0)))
            .unwrap()
    }

    fn mentions_of(&self, message_id: i64) -> Vec<(String, String)> {
        self.db
            .with_read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT e.kind, e.norm FROM entity_mentions m
                     JOIN entities e ON e.entity_id = m.entity_id
                     WHERE m.message_id = ?1 ORDER BY e.kind, e.norm",
                )?;
                let rows = stmt
                    .query_map([message_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
    }

    fn edge_weight(&self, a: &str, b: &str) -> Option<f64> {
        let (a, b) = (a.to_owned(), b.to_owned());
        self.db
            .with_read(move |c| {
                c.query_row(
                    "SELECT weight FROM entity_edges
                     WHERE rel = 'co_occurs'
                       AND src_id IN (SELECT entity_id FROM entities WHERE norm = ?1)
                       AND dst_id IN (SELECT entity_id FROM entities WHERE norm = ?2)",
                    rusqlite::params![a, b],
                    |r| r.get(0),
                )
                .optional()
            })
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

#[tokio::test]
async fn entities_and_mentions_are_recorded() {
    let fx = Fixture::open().await;
    let message_id = fx
        .message_with("Invoice INV-9 for £42.00, questions to ada@example.com")
        .await;

    let report = extract_entities(&fx.db, message_id).await.unwrap();

    assert_eq!(report.entities, 3);
    assert_eq!(report.mentions, 3);
    assert_eq!(
        fx.mentions_of(message_id),
        vec![
            ("amount".to_owned(), "GBP 42.00".to_owned()),
            ("email".to_owned(), "ada@example.com".to_owned()),
            ("invoice_id".to_owned(), "INV-9".to_owned()),
        ]
    );
}

#[tokio::test]
async fn the_same_thing_in_two_messages_is_one_entity() {
    // The normalized form is the identity. Without it the table becomes a list
    // of spellings rather than a list of things.
    let fx = Fixture::open().await;
    let first = fx.message_with("reply to Ada@Example.com").await;
    let second = fx.message_with("cc ADA@EXAMPLE.COM as well").await;

    extract_entities(&fx.db, first).await.unwrap();
    extract_entities(&fx.db, second).await.unwrap();

    assert_eq!(fx.entity_count(), 1, "one address, two spellings");
    assert_eq!(fx.mentions_of(first).len(), 1);
    assert_eq!(fx.mentions_of(second).len(), 1);
}

#[tokio::test]
async fn re_extraction_replaces_rather_than_accumulates() {
    // A body that lost a phone number must stop being findable by it.
    let fx = Fixture::open().await;
    let message_id = fx
        .message_with("call +1 555-010-1234 or mail ada@example.com")
        .await;
    extract_entities(&fx.db, message_id).await.unwrap();
    assert_eq!(fx.mentions_of(message_id).len(), 2);

    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE index_content SET text = 'mail ada@example.com' WHERE message_id = ?1",
                [message_id],
            )
        })
        .await
        .unwrap();
    extract_entities(&fx.db, message_id).await.unwrap();

    assert_eq!(
        fx.mentions_of(message_id),
        vec![("email".to_owned(), "ada@example.com".to_owned())],
        "the phone number is gone from this message"
    );
    assert_eq!(
        fx.entity_count(),
        2,
        "but the phone entity survives — another message may still refer to it"
    );
}

#[tokio::test]
async fn co_occurrence_edges_are_reinforced_not_duplicated() {
    let fx = Fixture::open().await;
    let first = fx.message_with("Invoice INV-9 from ada@example.com").await;
    extract_entities(&fx.db, first).await.unwrap();
    assert_eq!(fx.edge_weight("INV-9", "ada@example.com"), Some(1.0));

    let second = fx
        .message_with("Re: Invoice INV-9, thanks — ada@example.com")
        .await;
    extract_entities(&fx.db, second).await.unwrap();

    assert_eq!(
        fx.edge_weight("INV-9", "ada@example.com"),
        Some(2.0),
        "seeing them together twice is a stronger link, not a second one"
    );
}

#[tokio::test]
async fn an_edge_is_written_once_whichever_end_you_start_from() {
    let fx = Fixture::open().await;
    let message_id = fx.message_with("Invoice INV-9 from ada@example.com").await;
    extract_entities(&fx.db, message_id).await.unwrap();

    let edges: i64 = fx
        .db
        .with_read(|c| c.query_row("SELECT count(*) FROM entity_edges", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(edges, 1, "one undirected pair, not two directed ones");
}

#[tokio::test]
async fn a_repeated_address_is_one_fact_not_forty() {
    // A mailing-list footer would otherwise dominate the co-occurrence weights.
    let fx = Fixture::open().await;
    let body = "mail list@example.com. ".repeat(40);
    let message_id = fx.message_with(&body).await;

    let report = extract_entities(&fx.db, message_id).await.unwrap();

    assert_eq!(report.entities, 1);
    assert_eq!(
        report.mentions, MAX_MENTIONS_PER_PART,
        "exactly the cap: `<=` would also pass if the cap were one, or zero, \
         which is the failure this test exists to catch"
    );
}

#[tokio::test]
async fn an_empty_part_is_skipped_not_failed() {
    // A scanned PDF with no text layer is a normal thing to receive.
    let fx = Fixture::open().await;
    let message_id = fx.message_with("").await;

    let report = extract_entities(&fx.db, message_id).await.unwrap();

    assert_eq!(report.skipped_parts, 1);
    assert_eq!(report.mentions, 0);
}

#[tokio::test]
async fn a_message_that_was_never_extracted_is_a_failed_precondition() {
    // Silently recording nothing would look like a message with no entities,
    // which is a different and much quieter kind of wrong.
    let fx = Fixture::open().await;
    let message_id = fx
        .db
        .write({
            let (account_id, mailbox_id) = (fx.account_id, fx.mailbox_id);
            move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid: 900,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();

    let err = extract_entities(&fx.db, message_id).await.unwrap_err();
    assert_eq!(
        err.reason(),
        ErrorReason::FailedPrecondition,
        "the message exists; the pipeline is simply not far enough along, and \
         telling a client it is missing sends it after the wrong problem"
    );
}

#[tokio::test]
async fn deleting_a_message_takes_its_mentions_with_it() {
    let fx = Fixture::open().await;
    let message_id = fx.message_with("mail ada@example.com").await;
    extract_entities(&fx.db, message_id).await.unwrap();

    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [message_id]))
        .await
        .unwrap();

    let mentions: i64 = fx
        .db
        .with_read(|c| c.query_row("SELECT count(*) FROM entity_mentions", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(mentions, 0, "a mention of a message that is gone dangles");
    assert_eq!(
        fx.entity_count(),
        1,
        "the address itself is still a thing that exists"
    );

    // Nothing refers to it any more, though, and an entity table that only
    // grows is a table that eventually dominates the database.
    let reaped = collect_orphans(&fx.db).await.unwrap();
    assert_eq!(reaped, 1);
    assert_eq!(fx.entity_count(), 0);
}

#[tokio::test]
async fn an_entity_still_mentioned_elsewhere_survives_the_reaper() {
    let fx = Fixture::open().await;
    let kept = fx.message_with("mail ada@example.com").await;
    let dropped = fx.message_with("also mail ada@example.com").await;
    extract_entities(&fx.db, kept).await.unwrap();
    extract_entities(&fx.db, dropped).await.unwrap();

    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [dropped]))
        .await
        .unwrap();

    assert_eq!(collect_orphans(&fx.db).await.unwrap(), 0);
    assert_eq!(
        fx.entity_count(),
        1,
        "one message losing an address does not unmake it for the others"
    );
}

#[tokio::test]
async fn a_reply_that_names_entities_in_a_different_order_does_not_inflate_them() {
    // The fixture that broke the ±1 scheme, and the one every other edge test
    // was accidentally shaped to avoid. Entity ids are allocated in the textual
    // order of the message that *created* them, so a reply naming the same
    // entities in any other order used to withdraw from `(hi, lo)` while
    // contributing to `(lo, hi)`. The withdrawal wrote a phantom row that the
    // zero sweep deleted, and the real weight climbed on every redelivery.
    let fx = Fixture::open().await;
    let original = fx
        .message_with("Invoice INV-9 from ada@example.com about https://example.com/x")
        .await;
    extract_entities(&fx.db, original).await.unwrap();

    // Reversed: the reply mentions them in the opposite order to their ids.
    let reply = fx
        .message_with("https://example.com/x — ada@example.com re invoice INV-9")
        .await;
    extract_entities(&fx.db, reply).await.unwrap();

    let two = [
        fx.edge_weight("INV-9", "ada@example.com"),
        fx.edge_weight("INV-9", "https://example.com/x"),
        fx.edge_weight("ada@example.com", "https://example.com/x"),
    ];
    assert_eq!(two, [Some(2.0); 3], "seen together in two messages");

    // A lease expiring is routine, so redelivery is routine.
    for _ in 0..3 {
        extract_entities(&fx.db, reply).await.unwrap();
    }
    assert_eq!(
        [
            fx.edge_weight("INV-9", "ada@example.com"),
            fx.edge_weight("INV-9", "https://example.com/x"),
            fx.edge_weight("ada@example.com", "https://example.com/x"),
        ],
        two,
        "three redeliveries of unchanged content change nothing"
    );

    // And the withdrawal half: INV-9 leaves the reply.
    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE index_content SET text = 'https://example.com/x — ada@example.com'
                 WHERE message_id = ?1",
                [reply],
            )
        })
        .await
        .unwrap();
    extract_entities(&fx.db, reply).await.unwrap();
    assert_eq!(
        fx.edge_weight("INV-9", "ada@example.com"),
        Some(1.0),
        "one message still links them; the reply no longer does"
    );
}

#[tokio::test]
async fn an_edge_never_points_the_wrong_way() {
    // The schema enforces `src_id < dst_id`, so a writer that got the order
    // wrong fails loudly rather than creating a mirrored row that reads as a
    // second, separate relationship.
    let fx = Fixture::open().await;
    let message_id = fx.message_with("Invoice INV-9 from ada@example.com").await;
    extract_entities(&fx.db, message_id).await.unwrap();

    let wrong = fx
        .db
        .write(|c| {
            c.execute(
                "INSERT INTO entity_edges (src_id, dst_id, rel, weight)
                 SELECT dst_id, src_id, rel, weight FROM entity_edges",
                [],
            )
        })
        .await;
    assert!(wrong.is_err(), "the mirrored row must not be insertable");
}

#[tokio::test]
async fn deleting_a_message_withdraws_its_co_occurrence_weight() {
    // The mentions cascade away on their own; the weights they supported do
    // not, and a conversation that no longer exists must stop influencing the
    // ranking.
    let fx = Fixture::open().await;
    let kept = fx.message_with("Invoice INV-9 from ada@example.com").await;
    let doomed = fx.message_with("re invoice INV-9 — ada@example.com").await;
    extract_entities(&fx.db, kept).await.unwrap();
    extract_entities(&fx.db, doomed).await.unwrap();
    assert_eq!(fx.edge_weight("INV-9", "ada@example.com"), Some(2.0));

    forget_message(&fx.db, doomed).await.unwrap();
    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [doomed]))
        .await
        .unwrap();

    assert_eq!(
        fx.edge_weight("INV-9", "ada@example.com"),
        Some(1.0),
        "one message links them now, so the weight is one"
    );
}

#[tokio::test]
async fn an_expunge_withdraws_the_graph_contribution_too() {
    // The sync path deletes messages directly rather than through
    // `forget_message`, so it needs its own wiring — and mail the server says
    // is gone must stop influencing what search ranks first just as surely as
    // mail a user deleted.
    let fx = Fixture::open().await;
    let kept = fx.message_with("Invoice INV-9 from ada@example.com").await;
    let expunged = fx.message_with("re invoice INV-9 — ada@example.com").await;
    extract_entities(&fx.db, kept).await.unwrap();
    extract_entities(&fx.db, expunged).await.unwrap();
    assert_eq!(fx.edge_weight("INV-9", "ada@example.com"), Some(2.0));

    fx.db
        .write(move |c| crate::sync::remove_messages(c, &[expunged]))
        .await
        .unwrap();

    assert_eq!(
        fx.edge_weight("INV-9", "ada@example.com"),
        Some(1.0),
        "the expunged message no longer links them"
    );
}

#[tokio::test]
async fn forgetting_the_last_message_removes_the_edge_entirely() {
    let fx = Fixture::open().await;
    let message_id = fx.message_with("Invoice INV-9 from ada@example.com").await;
    extract_entities(&fx.db, message_id).await.unwrap();

    forget_message(&fx.db, message_id).await.unwrap();

    assert_eq!(fx.edge_weight("INV-9", "ada@example.com"), None);
    let edges: i64 = fx
        .db
        .with_read(|c| c.query_row("SELECT count(*) FROM entity_edges", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(edges, 0, "no evidence left, so no relationship left");
}

#[tokio::test]
async fn re_extraction_does_not_inflate_co_occurrence_weights() {
    // The queue redelivers on lease expiry, so re-running extraction over
    // unchanged content is routine rather than exceptional. An increment-only
    // edge write would drift upward every time a worker was slow, and the
    // ranking that reads these weights would quietly start preferring whichever
    // messages happened to be re-indexed most.
    let fx = Fixture::open().await;
    let message_id = fx.message_with("Invoice INV-9 from ada@example.com").await;

    for _ in 0..5 {
        extract_entities(&fx.db, message_id).await.unwrap();
    }

    assert_eq!(
        fx.edge_weight("INV-9", "ada@example.com"),
        Some(1.0),
        "five runs over one unchanged message is still one co-occurrence"
    );
}

#[tokio::test]
async fn an_entity_that_leaves_a_message_loses_its_edge() {
    let fx = Fixture::open().await;
    let message_id = fx.message_with("Invoice INV-9 from ada@example.com").await;
    extract_entities(&fx.db, message_id).await.unwrap();
    assert_eq!(fx.edge_weight("INV-9", "ada@example.com"), Some(1.0));

    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE index_content SET text = 'from ada@example.com' WHERE message_id = ?1",
                [message_id],
            )
        })
        .await
        .unwrap();
    extract_entities(&fx.db, message_id).await.unwrap();

    assert_eq!(
        fx.edge_weight("INV-9", "ada@example.com"),
        None,
        "the only evidence for the link is gone, so the link is gone"
    );
}

#[tokio::test]
async fn a_message_with_too_many_entities_is_truncated_rather_than_stalling() {
    // Edge writing is quadratic, and it runs inside the one writer connection
    // every other write in the process is queued behind. A link directory must
    // cost a bounded amount of that lock, not two minutes of it.
    let fx = Fixture::open().await;
    let mut body = String::new();
    for n in 0..(MAX_ENTITIES_PER_MESSAGE + 20) {
        body.push_str(&format!("user{n}@example.com "));
    }
    let message_id = fx.message_with(&body).await;

    let report = extract_entities(&fx.db, message_id).await.unwrap();

    assert_eq!(report.entities, MAX_ENTITIES_PER_MESSAGE);
    assert_eq!(
        report.truncated, 20,
        "distinct entities dropped, not mentions"
    );
    let edges: i64 = fx
        .db
        .with_read(|c| c.query_row("SELECT count(*) FROM entity_edges", [], |r| r.get(0)))
        .unwrap();
    let n = MAX_ENTITIES_PER_MESSAGE as i64;
    assert_eq!(edges, n * (n - 1) / 2, "every pair once, and no more");
}

#[tokio::test]
async fn many_large_parts_cost_a_bounded_amount_of_time_and_memory() {
    // A thirty-megabyte message is an ordinary mail size limit, not an attack.
    // Applying the entity cap after scanning meant every mention of every
    // entity was materialized first: 2.17 million of them, 707 MB resident and
    // 4.9 seconds inside an uncancellable `spawn_blocking`, to then keep 64.
    let fx = Fixture::open().await;
    let mut part = String::new();
    while part.len() < 900 * 1024 {
        part.push_str(&format!("user{}@example.com ", part.len()));
    }
    let message_id = fx.message_with("subject line").await;
    fx.db
        .write(move |c| {
            for n in 0..30 {
                c.execute(
                    "INSERT INTO index_content
                         (message_id, part, text, chars, content_hash, extractor)
                     VALUES (?1, ?2, ?3, ?4, X'00', 'test')",
                    rusqlite::params![
                        message_id,
                        format!("attachment:{n}"),
                        part,
                        part.len() as i64
                    ],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let report = extract_entities(&fx.db, message_id).await.unwrap();
    let elapsed = started.elapsed();

    assert_eq!(report.entities, MAX_ENTITIES_PER_MESSAGE);
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the budget must bind before the work is done, not after: {elapsed:?}"
    );
    assert_eq!(
        report.skipped_parts, 29,
        "once the cap is reached no later part can add an entity, so exactly \
         one part is scanned and the other twenty-nine are declined unread"
    );
    assert!(
        report.truncated <= MAX_TRUNCATION_TRACKED,
        "the record of what was dropped must be bounded too, got {}",
        report.truncated
    );
}

#[tokio::test]
async fn the_entity_budget_is_spent_on_the_subject_first() {
    // Truncation drops whatever arrives after the sixty-fourth entity, so the
    // order parts are read in decides what a link-heavy message stays findable
    // by. An invoice number in the subject is the single most searchable thing
    // in the message; alphabetical order spends the whole budget on
    // `attachment:` and `body` before reaching it.
    let fx = Fixture::open().await;
    let body: String = (0..80).map(|n| format!("body{n}@example.com ")).collect();
    let message_id = fx.message_with(&body).await;
    fx.db
        .write(move |c| {
            c.execute(
                "INSERT INTO index_content
                     (message_id, part, text, chars, content_hash, extractor)
                 VALUES (?1, 'subject', 'invoice INV-2024-0231', 21, X'00', 'test')",
                [message_id],
            )
        })
        .await
        .unwrap();

    extract_entities(&fx.db, message_id).await.unwrap();

    assert!(
        fx.mentions_of(message_id)
            .contains(&("invoice_id".to_owned(), "INV-2024-0231".to_owned())),
        "the subject's invoice number survived the cap: {:?}",
        fx.mentions_of(message_id)
    );
}

#[tokio::test]
async fn an_oversized_part_is_skipped_rather_than_scanned() {
    let fx = Fixture::open().await;
    let mut body = "x".repeat(MAX_SCAN_BYTES);
    body.push_str(" ada@example.com");
    let message_id = fx.message_with(&body).await;

    let report = extract_entities(&fx.db, message_id).await.unwrap();

    assert_eq!(report.skipped_parts, 1);
    assert_eq!(
        report.mentions, 0,
        "declined, not scanned — the address in it is the proof"
    );
}
