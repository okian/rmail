//! Invoice extraction: what is read, what is inferred, and the difference
//! between them surviving every hop.
//!
//! The tests that matter most here are not the happy path. They are:
//!
//! - the ones that prove a *parsed* field never comes back looking inferred
//!   and vice versa — through the merge, through SQLite, and into the CSV;
//! - the ones that prove a wrong reading is *reported* rather than reconciled
//!   (`subtotal_plus_tax_that_misses_the_total_is_a_warning`);
//! - the ones that reach each named bound, because an invoice is a document a
//!   stranger sent in order to be paid;
//! - `amount_paid_is_not_a_payment_status`, which is the single worst mistake
//!   available: filing a live debt as settled.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::extract::store::{self, InvoiceFilter};
use crate::extract::tables;
use crate::storage::Database;
use crate::{repo, ErrorReason};

// ---------------------------------------------------------------------------
// The deterministic reader
// ---------------------------------------------------------------------------

/// An invoice with every field a document normally labels.
const SAMPLE: &str = "\
Acme Consulting Ltd
Vendor: Acme Consulting Ltd
Invoice Number: INV-2024-0231
Invoice date: 2024-03-01
Due date: 2024-03-31
Subtotal: $1,200.00
Tax: $99.00
Total: $1,299.00
";

fn parse(text: &str) -> Invoice {
    parse_document(DocKind::Invoice, "3", text)
}

#[test]
fn a_labelled_total_is_parsed_and_its_span_points_at_the_amount() {
    let invoice = parse(SAMPLE);
    let total = invoice.total.expect("a total");
    assert_eq!(total.value.minor_units, 129_900);
    assert_eq!(total.value.currency, "USD");
    assert_eq!(total.provenance.origin, Origin::Parsed);
    assert_eq!(total.provenance.part, "3");
    // The span has to name the amount, not the line: it is the only thing that
    // makes the claim checkable against the document.
    let slice = SAMPLE
        .get(total.provenance.span_start..total.provenance.span_end)
        .expect("the span is inside the document");
    assert_eq!(slice, "$1,299.00");
}

#[test]
fn subtotal_is_not_read_as_total() {
    let invoice = parse(SAMPLE);
    assert_eq!(
        invoice.subtotal.expect("a subtotal").value.minor_units,
        120_000
    );
    assert_eq!(invoice.tax.expect("a tax").value.minor_units, 9_900);
    assert_eq!(invoice.total.expect("a total").value.minor_units, 129_900);
}

#[test]
fn several_labels_on_one_line_are_all_read() {
    let invoice = parse("Subtotal $1,200.00  Tax $99.00  Total $1,299.00\n");
    assert_eq!(
        invoice.subtotal.expect("subtotal").value.minor_units,
        120_000
    );
    assert_eq!(invoice.tax.expect("tax").value.minor_units, 9_900);
    assert_eq!(invoice.total.expect("total").value.minor_units, 129_900);
}

#[test]
fn the_first_statement_of_a_total_wins() {
    // A remittance slip at the foot of an invoice repeats the total, sometimes
    // wrongly. The document's own total block is the authority.
    let invoice = parse("Total: $1,299.00\n\nRemittance slip\nTotal due: $9,999.00\n");
    assert_eq!(invoice.total.expect("total").value.minor_units, 129_900);
}

#[test]
fn the_number_comes_from_the_reference_extractor() {
    let invoice = parse(SAMPLE);
    let number = invoice.number.expect("a number");
    assert_eq!(number.value, "INV-2024-0231");
    assert_eq!(number.provenance.origin, Origin::Parsed);
}

#[test]
fn a_word_that_follows_the_label_is_not_a_number() {
    // `index::entities::identifier_shaped` is what rejects this, and this test
    // is here to prove that guard is actually on the path an invoice takes.
    let invoice = parse("Please find the invoice attached.\nTotal: $10.00\n");
    assert!(invoice.number.is_none(), "{:?}", invoice.number);
}

#[test]
fn dates_are_taken_from_their_labels() {
    let invoice = parse(SAMPLE);
    let issued = invoice.issued_at.expect("issued").value;
    let due = invoice.due_at.expect("due").value;
    assert_eq!(
        chrono::DateTime::from_timestamp(issued, 0)
            .map(|at| at.format("%Y-%m-%d").to_string())
            .as_deref(),
        Some("2024-03-01")
    );
    assert_eq!(
        chrono::DateTime::from_timestamp(due, 0)
            .map(|at| at.format("%Y-%m-%d").to_string())
            .as_deref(),
        Some("2024-03-31")
    );
}

#[test]
fn a_vendor_is_only_claimed_from_an_explicit_label() {
    let labelled = parse(SAMPLE);
    assert_eq!(
        labelled.vendor.expect("vendor").value,
        "Acme Consulting Ltd"
    );

    // The same letterhead with no label claims nothing: a vendor this reader
    // cannot prove is one the model route may infer, and it will say so.
    let unlabelled = parse("Acme Consulting Ltd\nInvoice\nTotal: $10.00\n");
    assert!(unlabelled.vendor.is_none(), "{:?}", unlabelled.vendor);
}

#[test]
fn unpaid_is_not_read_as_paid() {
    let invoice = parse("Status: unpaid\nTotal: $10.00\n");
    assert_eq!(invoice.status.expect("status").value, PaymentStatus::Unpaid);
}

#[test]
fn amount_paid_is_not_a_payment_status_whatever_separates_the_two_words() {
    // The blocker has to survive a separator that is not a space. A soft
    // hyphen and a zero-width space are ordinary PDF-to-text output, neither is
    // `char::is_whitespace`, and reading `Amount<sep>paid: 0.00` as PAID files
    // a live debt as settled.
    for separator in [" ", "\u{00AD}", "\u{200B}", "\u{2013}", "  "] {
        let text = format!("Amount{separator}paid: $0.00\nBalance due: $1,299.00\n");
        let invoice = parse(&text);
        assert!(
            invoice.status.is_none(),
            "separator {separator:?}: {:?}",
            invoice.status
        );
    }
    // The mirror: a word that is not a blocker still yields a status, so the
    // guard above is narrow rather than a blanket refusal.
    assert_eq!(
        parse("Invoice paid\nTotal: $10.00\n")
            .status
            .expect("status")
            .value,
        PaymentStatus::Paid
    );
}

#[test]
fn amount_paid_is_not_a_payment_status() {
    // The worst available mistake: an invoice that prints `Amount paid: 0.00`
    // is a live debt, and reading that word as a stamp would file it settled.
    let invoice = parse("Amount paid: $0.00\nBalance due: $1,299.00\n");
    assert!(invoice.status.is_none(), "{:?}", invoice.status);
    assert_eq!(invoice.total.expect("total").value.minor_units, 129_900);
}

#[test]
fn a_stamped_receipt_is_paid() {
    let invoice = parse_document(DocKind::Receipt, "", "PAID\nTotal: $10.00\n");
    assert_eq!(invoice.status.expect("status").value, PaymentStatus::Paid);
}

#[test]
fn page_markers_travel_into_provenance() {
    let text = "[page 1]\nInvoice\n[page 2]\nTotal: $10.00\n";
    let invoice = parse_document(DocKind::Invoice, "0", text);
    assert_eq!(invoice.total.expect("total").provenance.page, Some(2));
}

#[test]
fn subtotal_plus_tax_that_misses_the_total_is_a_warning_and_nothing_is_adjusted() {
    let invoice = parse("Subtotal: $100.00\nTax: $10.00\nTotal: $200.00\n");
    assert_eq!(invoice.total.expect("total").value.minor_units, 20_000);
    assert_eq!(
        invoice.subtotal.expect("subtotal").value.minor_units,
        10_000
    );
    assert!(
        invoice
            .warnings
            .iter()
            .any(|warning| warning.contains("not the stated total")),
        "{:?}",
        invoice.warnings
    );
}

#[test]
fn amounts_in_different_currencies_are_not_cross_checked() {
    let invoice = parse("Subtotal: $100.00\nTax: £10.00\nTotal: $110.00\n");
    assert!(
        invoice
            .warnings
            .iter()
            .any(|warning| warning.contains("same currency")),
        "{:?}",
        invoice.warnings
    );
}

#[test]
fn european_separators_are_read_by_the_shared_amount_parser() {
    let invoice = parse("Total: EUR 1.299,50\n");
    let total = invoice.total.expect("total");
    assert_eq!(total.value.currency, "EUR");
    assert_eq!(total.value.minor_units, 129_950);
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

#[test]
fn a_document_with_no_money_is_never_a_bill() {
    // "Please see the attached invoice" — a covering note. Extracting it would
    // put an all-empty row in a table people query.
    assert_eq!(
        detect(Some("invoice.pdf"), None, "Invoice\nAmount due\nDue date\n"),
        None
    );
}

#[test]
fn an_ordinary_email_is_neither() {
    assert_eq!(
        detect(
            None,
            None,
            "Lunch on Tuesday? It cost me $12.00 last time.\n"
        ),
        None
    );
}

#[test]
fn a_filename_that_says_invoice_counts_towards_the_score() {
    let text = "Acme Ltd\nBill to: Ada\nTotal: $10.00\n";
    assert_eq!(detect(None, None, text), None);
    assert_eq!(
        detect(Some("invoice-2291.pdf"), None, text),
        Some(DocKind::Invoice)
    );
}

#[test]
fn a_receipt_outscores_an_invoice_when_it_says_so() {
    let text = "Receipt\nPayment received\nThank you for your purchase\nTotal: $10.00\n";
    assert_eq!(detect(None, None, text), Some(DocKind::Receipt));
}

#[test]
fn a_tie_is_read_as_an_invoice() {
    // A receipt filed as an invoice is a paid bill in a list of bills; an
    // invoice filed as a receipt is a debt recorded as settled.
    let text = "Invoice\nAmount due: $10.00\nReceipt\nPayment received\n";
    assert_eq!(detect(None, None, text), Some(DocKind::Invoice));
}

// ---------------------------------------------------------------------------
// Bounds — each one reached
// ---------------------------------------------------------------------------

#[test]
fn a_document_past_the_byte_bound_is_truncated_and_says_so() {
    let mut text = "Total: $10.00\n".to_owned();
    text.push_str(&"filler line\n".repeat(MAX_DOCUMENT_BYTES / 12 + 8));
    let invoice = parse(&text);
    assert!(text.len() > MAX_DOCUMENT_BYTES);
    assert!(
        invoice
            .warnings
            .iter()
            .any(|warning| warning.contains("bytes of this document")),
        "{:?}",
        invoice.warnings
    );
}

#[test]
fn a_document_past_the_line_bound_stops_reading() {
    let mut text = "x\n".repeat(MAX_LINES + 10);
    text.push_str("Total: $10.00\n");
    let invoice = parse(&text);
    assert!(invoice.total.is_none(), "the tail should not be read");
    assert!(
        invoice
            .warnings
            .iter()
            .any(|warning| warning.contains("lines were read")),
        "{:?}",
        invoice.warnings
    );
}

#[test]
fn a_line_stops_being_read_at_the_label_bound() {
    // Exactly enough `tax` labels to exhaust the per-line budget, and then a
    // `total`. The total is past the cap, so it must not be read — which is
    // what makes this a test of the bound rather than a restatement of it.
    let mut line = "tax $1.00 ".repeat(MAX_LABELS_PER_LINE);
    line.push_str("total $99.00");
    let invoice = parse(&format!("{line}\n"));
    assert_eq!(invoice.tax.expect("tax").value.minor_units, 100);
    assert!(invoice.total.is_none(), "{:?}", invoice.total);

    // One fewer, and the same total is reached: the cap is the only thing
    // stopping it above.
    let mut shorter = "tax $1.00 ".repeat(MAX_LABELS_PER_LINE - 1);
    shorter.push_str("total $99.00");
    let reached = parse(&format!("{shorter}\n"));
    assert_eq!(reached.total.expect("total").value.minor_units, 9_900);
}

#[test]
fn a_field_past_the_byte_bound_is_cut_on_a_character_boundary() {
    let long = "é".repeat(MAX_FIELD_BYTES);
    let invoice = parse(&format!("Vendor: {long}\nTotal: $1.00\n"));
    let vendor = invoice.vendor.expect("vendor").value;
    assert!(vendor.len() <= MAX_FIELD_BYTES);
    // The real assertion: it is still a `String`, so the cut landed on a
    // character boundary rather than mid-`é`.
    assert!(vendor.chars().all(|c| c == 'é'));
}

#[test]
fn a_multibyte_document_at_every_offset_never_panics() {
    // The failure this guards against is a byte-slice landing inside a
    // multi-byte character, which shipped twice in task 75.
    for pad in 0..8 {
        let text = format!("{}Total: €1,299,00 — paid\n", "é".repeat(pad));
        let invoice = parse(&text);
        assert!(invoice.warnings.len() < 10);
    }
}

// ---------------------------------------------------------------------------
// Money parsing
// ---------------------------------------------------------------------------

#[test]
fn a_bare_number_needs_a_fallback_currency() {
    assert_eq!(parse_money("1,299.00", None), None);
    assert_eq!(
        parse_money("1,299.00", Some("GBP")),
        Some(Money {
            currency: "GBP".to_owned(),
            minor_units: 129_900,
        })
    );
}

#[test]
fn accounting_negatives_are_credits() {
    for text in ["(42.00)", "-42.00"] {
        assert_eq!(
            parse_money(text, Some("USD")),
            Some(Money {
                currency: "USD".to_owned(),
                minor_units: -4_200,
            }),
            "{text}"
        );
    }
}

#[test]
fn a_marked_amount_ignores_the_fallback() {
    assert_eq!(
        parse_money("£42.00", Some("USD")),
        Some(Money {
            currency: "GBP".to_owned(),
            minor_units: 4_200,
        })
    );
}

#[test]
fn money_renders_negatives_and_pennies() {
    assert_eq!(
        Money {
            currency: "USD".to_owned(),
            minor_units: -5,
        }
        .display(),
        "-0.05 USD"
    );
}

// ---------------------------------------------------------------------------
// Line items out of a native table
// ---------------------------------------------------------------------------

fn invoice_table() -> tables::Table {
    let csv = "Description,Quantity,Unit Price,Amount\n\
               Consulting,10,120.00,1200.00\n\
               Support,0.5,200.00,100.00\n";
    let report = tables::from_csv("lines.csv", csv).expect("csv");
    report.tables.into_iter().next().expect("one table")
}

#[test]
fn line_items_come_out_of_the_grid_and_are_parsed_not_inferred() {
    let items = line_items_from_table(&invoice_table(), Some("USD"));
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].description, "Consulting");
    assert_eq!(items[0].quantity, Some(10.0));
    assert_eq!(
        items[0].total,
        Some(Money {
            currency: "USD".to_owned(),
            minor_units: 120_000,
        })
    );
    assert_eq!(items[1].quantity, Some(0.5));
    assert!(items.iter().all(|item| item.origin == Origin::Parsed));
}

#[test]
fn a_quantity_cell_can_never_produce_a_non_finite_number() {
    // `"inf".parse::<f64>()` succeeds. A cell reading `inf` would put a
    // non-finite double into SQLite's REAL column and onto the wire, where
    // every consumer that multiplies by it produces NaN.
    let csv = "Description,Quantity,Amount\n               Widget,inf,1.00\n               Gadget,NaN,1.00\n               Bolt,-1e400,1.00\n";
    let report = tables::from_csv("lines.csv", csv).expect("csv");
    let table = report.tables.into_iter().next().expect("one table");
    let items = line_items_from_table(&table, Some("USD"));
    assert_eq!(items.len(), 3);
    for item in &items {
        assert!(
            item.quantity.is_none_or(f64::is_finite),
            "{}: {:?}",
            item.description,
            item.quantity
        );
    }
}

#[test]
fn a_table_with_no_money_column_is_not_a_line_item_table() {
    let csv = "Name,Address\nAda,1 Main St\n";
    let report = tables::from_csv("addresses.csv", csv).expect("csv");
    let table = report.tables.into_iter().next().expect("one table");
    assert!(line_items_from_table(&table, Some("USD")).is_empty());
}

#[test]
fn line_items_stop_at_the_bound() {
    let mut csv = "Description,Amount\n".to_owned();
    for n in 0..(MAX_LINE_ITEMS + 50) {
        csv.push_str(&format!("Item {n},1.00\n"));
    }
    let report = tables::from_csv("lines.csv", &csv).expect("csv");
    let table = report.tables.into_iter().next().expect("one table");
    assert_eq!(
        line_items_from_table(&table, Some("USD")).len(),
        MAX_LINE_ITEMS
    );
}

// ---------------------------------------------------------------------------
// The model route
// ---------------------------------------------------------------------------

fn model_answer() -> String {
    serde_json::json!({
        "document_kind": "invoice",
        "vendor": "Globex",
        "number": "G-77",
        "currency": "EUR",
        "issued_date": "2024-05-02",
        "due_date": "",
        "subtotal": "",
        "tax": "",
        "total": "1.234,50",
        "status": "unpaid",
        "line_items": [
            {"description": "Widget", "quantity": "2", "unit_price": "617,25", "total": "1.234,50"}
        ],
    })
    .to_string()
}

#[test]
fn every_model_field_is_marked_inferred() {
    let invoice = from_model_answer(DocKind::Invoice, "0", &model_answer()).expect("answer");
    assert_eq!(invoice.vendor.as_ref().expect("vendor").value, "Globex");
    assert!(invoice.vendor.expect("vendor").inferred());
    assert!(invoice.number.expect("number").inferred());
    assert!(invoice.total.as_ref().expect("total").inferred());
    assert!(invoice.issued_at.expect("issued").inferred());
    assert!(invoice.status.expect("status").inferred());
    assert!(invoice
        .line_items
        .iter()
        .all(|item| item.origin == Origin::Model));
}

#[test]
fn a_model_amount_uses_the_answers_own_currency() {
    let invoice = from_model_answer(DocKind::Invoice, "0", &model_answer()).expect("answer");
    let total = invoice.total.expect("total");
    assert_eq!(total.value.currency, "EUR");
    assert_eq!(total.value.minor_units, 123_450);
}

#[test]
fn a_model_field_carries_no_invented_span() {
    let invoice = from_model_answer(DocKind::Invoice, "7", &model_answer()).expect("answer");
    let provenance = invoice.total.expect("total").provenance;
    assert_eq!(provenance.part, "7");
    assert_eq!((provenance.span_start, provenance.span_end), (0, 0));
    assert_eq!(provenance.page, None);
}

#[test]
fn an_answer_that_is_not_the_schema_is_an_error_not_a_partial_invoice() {
    let error = from_model_answer(DocKind::Invoice, "0", "{\"line_items\": 7}")
        .expect_err("a wrong shape must not become an invoice");
    assert_eq!(error.reason(), ErrorReason::Internal);
}

#[test]
fn a_status_this_build_cannot_name_is_left_unset() {
    let answer = serde_json::json!({
        "document_kind": "", "vendor": "", "number": "", "currency": "",
        "issued_date": "", "due_date": "", "subtotal": "", "tax": "",
        "total": "", "status": "probably-paid?", "line_items": [],
    })
    .to_string();
    let invoice = from_model_answer(DocKind::Invoice, "0", &answer).expect("answer");
    assert!(invoice.status.is_none(), "{:?}", invoice.status);
}

#[test]
fn model_line_items_stop_at_the_bound_and_say_so() {
    let items: Vec<serde_json::Value> = (0..(MAX_LINE_ITEMS + 20))
        .map(|n| {
            serde_json::json!({
                "description": format!("Item {n}"),
                "quantity": "", "unit_price": "", "total": "1.00",
            })
        })
        .collect();
    let answer = serde_json::json!({
        "document_kind": "", "vendor": "", "number": "", "currency": "USD",
        "issued_date": "", "due_date": "", "subtotal": "", "tax": "",
        "total": "", "status": "", "line_items": items,
    })
    .to_string();
    let invoice = from_model_answer(DocKind::Invoice, "0", &answer).expect("answer");
    assert_eq!(invoice.line_items.len(), MAX_LINE_ITEMS);
    assert!(invoice
        .warnings
        .iter()
        .any(|warning| warning.contains("line items were kept")));
}

// ---------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------

#[test]
fn a_parsed_total_survives_a_model_that_disagrees_and_the_disagreement_is_reported() {
    let parsed = parse("Total: $1,299.00\n");
    let model = from_model_answer(DocKind::Invoice, "0", &model_answer()).expect("answer");
    let merged = merge(parsed, model);

    let total = merged.total.as_ref().expect("total");
    assert_eq!(total.value.minor_units, 129_900);
    assert_eq!(total.provenance.origin, Origin::Parsed);
    assert!(
        merged
            .warnings
            .iter()
            .any(|warning| warning.contains("the model read")),
        "{:?}",
        merged.warnings
    );
}

#[test]
fn the_model_fills_only_what_the_document_did_not_state() {
    let parsed = parse("Total: $1,299.00\n");
    assert!(parsed.vendor.is_none());
    let model = from_model_answer(DocKind::Invoice, "0", &model_answer()).expect("answer");
    let merged = merge(parsed, model);

    let vendor = merged.vendor.expect("vendor from the model");
    assert_eq!(vendor.value, "Globex");
    assert!(vendor.inferred());
    // And the currency still comes from the parsed total, not the model's.
    assert_eq!(merged.currency.as_deref(), Some("USD"));
}

#[test]
fn parsed_line_items_are_not_replaced_by_a_models() {
    let mut parsed = parse("Total: $1,299.00\n");
    parsed.line_items = line_items_from_table(&invoice_table(), Some("USD"));
    let model = from_model_answer(DocKind::Invoice, "0", &model_answer()).expect("answer");
    let merged = merge(parsed, model);
    assert_eq!(merged.line_items.len(), 2);
    assert!(merged
        .line_items
        .iter()
        .all(|item| item.origin == Origin::Parsed));
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

fn stored(invoice: Invoice) -> StoredInvoice {
    StoredInvoice {
        invoice_id: 1,
        message_id: 2,
        extracted_at: 1_700_000_000,
        invoice,
    }
}

#[test]
fn the_csv_names_every_inferred_field() {
    let parsed = parse("Total: $1,299.00\n");
    let model = from_model_answer(DocKind::Invoice, "0", &model_answer()).expect("answer");
    let csv = to_csv(&[stored(merge(parsed, model))]);

    let header = csv.lines().next().expect("a header");
    assert_eq!(header, CSV_COLUMNS.join(","));
    let row = csv.lines().nth(1).expect("a row");
    // Read the column by position rather than by substring: a substring test
    // would pass on a row that merely mentioned the word somewhere.
    let column = CSV_COLUMNS
        .iter()
        .position(|name| *name == "inferred_fields")
        .expect("the column exists");
    let cell = row.split(',').nth(column).expect("the cell");
    // `total` was parsed and must not appear; everything the model supplied
    // must — including `line_items`, which lives outside
    // `Invoice::provenance()` and would otherwise be silently omitted while
    // `Invoice::inferred()` still counted it.
    assert_eq!(cell, "issued_at number status vendor line_items", "{row}");
}

#[test]
fn a_vendor_that_is_a_formula_cannot_execute_in_a_spreadsheet() {
    let mut invoice = parse("Total: $1.00\n");
    invoice.vendor = Some(Claim {
        value: "=cmd|' /c calc'!A0".to_owned(),
        provenance: Provenance::default(),
    });
    let csv = to_csv(&[stored(invoice)]);
    assert!(csv.contains("'=cmd|"), "{csv}");
    assert!(!csv.contains(",=cmd|"), "{csv}");
}

#[test]
fn csv_fields_with_commas_and_quotes_are_quoted() {
    assert_eq!(csv_field("Acme, Ltd"), "\"Acme, Ltd\"");
    assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    assert_eq!(csv_field("plain"), "plain");
    // A leading minus is a formula in Excel *and* a legitimate credit, so it
    // is escaped and then quoted because the escape adds no comma.
    assert_eq!(csv_field("-42.00"), "'-42.00");
}

#[test]
fn csv_money_is_a_bare_decimal_a_spreadsheet_can_sum() {
    let csv = to_csv(&[stored(parse("Total: $1,299.00\n"))]);
    let row = csv.lines().nth(1).expect("a row");
    assert!(row.contains(",1299.00,"), "{row}");
    assert!(row.contains(",USD,"), "{row}");
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

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
        let path = std::env::temp_dir().join(format!("rmail-extract-invoice-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open");
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
            .expect("seed");
        Self {
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    async fn message(&self) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        self.db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some("Invoice".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .await
            .expect("message")
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
async fn a_stored_invoice_keeps_every_field_and_its_provenance() {
    let fixture = Fixture::open().await;
    let message_id = fixture.message().await;
    let parsed = parse(SAMPLE);
    let model = from_model_answer(DocKind::Invoice, "3", &model_answer()).expect("answer");
    let merged = merge(parsed, model);

    let saved = store::save_invoice(&fixture.db, message_id, &merged)
        .await
        .expect("save");
    assert!(saved.invoice_id > 0);

    let rows = store::list_invoices(
        &fixture.db,
        &InvoiceFilter {
            message_id: Some(message_id),
            ..InvoiceFilter::default()
        },
    )
    .await
    .expect("list");
    assert_eq!(rows.len(), 1);
    let read = &rows[0].invoice;
    assert_eq!(read.kind, DocKind::Invoice);
    assert_eq!(read.part, "3");
    assert_eq!(
        read.vendor.as_ref().expect("vendor").value,
        "Acme Consulting Ltd"
    );
    assert_eq!(
        read.total.as_ref().expect("total").value.minor_units,
        129_900
    );
    // The distinction has to survive SQLite: the vendor was read off a
    // labelled line, so it must not come back inferred.
    assert_eq!(
        read.vendor.as_ref().expect("vendor").provenance.origin,
        Origin::Parsed
    );
    assert_eq!(
        read.total.as_ref().expect("total").provenance.span_start,
        merged.total.as_ref().expect("total").provenance.span_start
    );
    assert!(read.warnings.iter().any(|w| w.contains("the model read")));
}

#[tokio::test]
async fn re_extracting_a_document_replaces_it_rather_than_accumulating() {
    let fixture = Fixture::open().await;
    let message_id = fixture.message().await;

    let first = parse("Total: $10.00\n");
    store::save_invoice(&fixture.db, message_id, &first)
        .await
        .expect("first");
    let second = parse("Total: $20.00\n");
    store::save_invoice(&fixture.db, message_id, &second)
        .await
        .expect("second");

    let rows = store::list_invoices(&fixture.db, &InvoiceFilter::default())
        .await
        .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]
            .invoice
            .total
            .as_ref()
            .expect("total")
            .value
            .minor_units,
        2_000
    );
}

#[tokio::test]
async fn re_extraction_does_not_leave_the_previous_readings_line_items_behind() {
    let fixture = Fixture::open().await;
    let message_id = fixture.message().await;

    let mut first = parse("Total: $10.00\n");
    first.line_items = line_items_from_table(&invoice_table(), Some("USD"));
    store::save_invoice(&fixture.db, message_id, &first)
        .await
        .expect("first");

    let second = parse("Total: $20.00\n");
    assert!(second.line_items.is_empty());
    store::save_invoice(&fixture.db, message_id, &second)
        .await
        .expect("second");

    let rows = store::list_invoices(&fixture.db, &InvoiceFilter::default())
        .await
        .expect("list");
    assert!(rows[0].invoice.line_items.is_empty(), "{:?}", rows[0]);
}

#[tokio::test]
async fn line_items_round_trip_with_their_origin() {
    let fixture = Fixture::open().await;
    let message_id = fixture.message().await;

    let mut invoice = parse("Total: $1,300.00\n");
    invoice.line_items = line_items_from_table(&invoice_table(), Some("USD"));
    store::save_invoice(&fixture.db, message_id, &invoice)
        .await
        .expect("save");

    let rows = store::list_invoices(&fixture.db, &InvoiceFilter::default())
        .await
        .expect("list");
    let items = &rows[0].invoice.line_items;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].description, "Consulting");
    assert_eq!(items[0].quantity, Some(10.0));
    assert_eq!(
        items[0].total,
        Some(Money {
            currency: "USD".to_owned(),
            minor_units: 120_000,
        })
    );
    assert!(items.iter().all(|item| item.origin == Origin::Parsed));
}

#[tokio::test]
async fn an_extraction_that_found_nothing_is_refused_rather_than_stored_empty() {
    let fixture = Fixture::open().await;
    let message_id = fixture.message().await;
    let nothing = Invoice::empty(DocKind::Invoice, "0");
    let error = store::save_invoice(&fixture.db, message_id, &nothing)
        .await
        .expect_err("an all-empty row is a fiction");
    assert_eq!(error.reason(), ErrorReason::FailedPrecondition);
}

#[tokio::test]
async fn the_filters_narrow_by_vendor_account_and_issue_date() {
    let fixture = Fixture::open().await;
    let acme = fixture.message().await;
    let globex = fixture.message().await;

    store::save_invoice(
        &fixture.db,
        acme,
        &parse("Vendor: Acme Ltd\nInvoice date: 2024-01-05\nTotal: $10.00\n"),
    )
    .await
    .expect("acme");
    store::save_invoice(
        &fixture.db,
        globex,
        &parse("Vendor: Globex\nInvoice date: 2024-06-05\nTotal: $20.00\n"),
    )
    .await
    .expect("globex");

    let by_vendor = store::list_invoices(
        &fixture.db,
        &InvoiceFilter {
            vendor: Some("acme".to_owned()),
            ..InvoiceFilter::default()
        },
    )
    .await
    .expect("vendor");
    assert_eq!(by_vendor.len(), 1);
    assert_eq!(
        by_vendor[0].invoice.vendor.as_ref().expect("vendor").value,
        "Acme Ltd"
    );

    let since = chrono::NaiveDate::from_ymd_opt(2024, 3, 1)
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|at| at.and_utc().timestamp())
        .expect("a day");
    let recent = store::list_invoices(
        &fixture.db,
        &InvoiceFilter {
            since: Some(since),
            ..InvoiceFilter::default()
        },
    )
    .await
    .expect("since");
    assert_eq!(recent.len(), 1);
    assert_eq!(
        recent[0].invoice.vendor.as_ref().expect("vendor").value,
        "Globex"
    );

    let other_account = store::list_invoices(
        &fixture.db,
        &InvoiceFilter {
            account_id: Some(fixture.account_id + 1),
            ..InvoiceFilter::default()
        },
    )
    .await
    .expect("account");
    assert!(other_account.is_empty());
}

#[tokio::test]
async fn the_page_size_is_honoured_and_an_absurd_one_is_survived() {
    let fixture = Fixture::open().await;
    for n in 0..3 {
        let message_id = fixture.message().await;
        store::save_invoice(
            &fixture.db,
            message_id,
            &parse(&format!("Total: ${n}.00\n")),
        )
        .await
        .expect("save");
    }

    let page = store::list_invoices(
        &fixture.db,
        &InvoiceFilter {
            limit: 2,
            ..InvoiceFilter::default()
        },
    )
    .await
    .expect("list");
    assert_eq!(page.len(), 2);

    // An absurd limit is clamped rather than passed through, where it would
    // otherwise reach SQLite as a value the ceiling exists to prevent.
    let everything = store::list_invoices(
        &fixture.db,
        &InvoiceFilter {
            limit: i64::MAX,
            ..InvoiceFilter::default()
        },
    )
    .await
    .expect("list");
    assert_eq!(everything.len(), 3);
}

// ---------------------------------------------------------------------------
// One row, one currency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_amount_in_another_currency_is_not_stored_under_this_rows_currency() {
    // `invoices` has one `currency` column. Storing a £ tax next to a $ total
    // and re-labelling it on read would turn a mismatch this module already
    // warns about into a silently wrong figure — the exact thing the table
    // exists to prevent.
    let fixture = Fixture::open().await;
    let message_id = fixture.message().await;
    let invoice = parse("Subtotal: $100.00\nTax: £10.00\nTotal: $110.00\n");
    assert_eq!(invoice.tax.as_ref().expect("tax").value.currency, "GBP");

    store::save_invoice(&fixture.db, message_id, &invoice)
        .await
        .expect("save");
    let rows = store::list_invoices(&fixture.db, &InvoiceFilter::default())
        .await
        .expect("list");
    let read = &rows[0].invoice;
    assert_eq!(read.currency.as_deref(), Some("USD"));
    assert!(read.tax.is_none(), "a GBP tax must not come back as USD");
    assert_eq!(
        read.total.as_ref().expect("total").value.minor_units,
        11_000
    );
    assert!(
        read.warnings
            .iter()
            .any(|warning| warning.contains("not in this document's currency")),
        "{:?}",
        read.warnings
    );
}

#[test]
fn a_csv_amount_in_another_currency_stops_being_summable() {
    let invoice = parse("Subtotal: $100.00\nTax: £10.00\nTotal: $110.00\n");
    let csv = to_csv(&[stored(invoice)]);
    let row = csv.lines().nth(1).expect("a row");
    let column = CSV_COLUMNS
        .iter()
        .position(|name| *name == "tax")
        .expect("the column exists");
    // A bare `10.00` here would be summed alongside dollars by any
    // spreadsheet, silently.
    assert_eq!(row.split(',').nth(column), Some("10.00 GBP"), "{row}");
}

#[tokio::test]
async fn a_row_whose_provenance_is_missing_reads_back_as_inferred() {
    // `Origin::default()` is `Parsed`, which is right for a producer and
    // exactly wrong on the read path: a row that does not say where a field
    // came from must not present it as the document's own words.
    let fixture = Fixture::open().await;
    let message_id = fixture.message().await;
    store::save_invoice(&fixture.db, message_id, &parse(SAMPLE))
        .await
        .expect("save");
    fixture
        .db
        .write(|c| c.execute("UPDATE invoices SET provenance = '{}'", []))
        .await
        .expect("blank the provenance");

    let rows = store::list_invoices(&fixture.db, &InvoiceFilter::default())
        .await
        .expect("list");
    let read = &rows[0].invoice;
    for claim in [
        read.vendor.as_ref().map(|c| &c.provenance),
        read.number.as_ref().map(|c| &c.provenance),
        read.total.as_ref().map(|c| &c.provenance),
    ] {
        assert_eq!(
            claim.expect("a claim").origin,
            Origin::Model,
            "an unrecorded origin must understate, never overstate"
        );
    }
    assert!(read.inferred());
}

#[tokio::test]
async fn a_row_whose_provenance_will_not_decode_reads_back_as_inferred() {
    let fixture = Fixture::open().await;
    let message_id = fixture.message().await;
    store::save_invoice(&fixture.db, message_id, &parse(SAMPLE))
        .await
        .expect("save");
    fixture
        .db
        .write(|c| c.execute("UPDATE invoices SET provenance = 'not json'", []))
        .await
        .expect("corrupt the provenance");

    let rows = store::list_invoices(&fixture.db, &InvoiceFilter::default())
        .await
        .expect("list");
    assert!(rows[0].invoice.inferred());
}

// ---------------------------------------------------------------------------
// The engine, on a daemon with no provider
// ---------------------------------------------------------------------------

/// An engine with no model at all — the configuration a daemon with AI off
/// actually runs, and the only place the no-provider codes can be pinned.
fn engine(db: &Database) -> crate::extract::ExtractEngine {
    crate::extract::ExtractEngine::new(db.clone(), None, crate::config::ExtractConfig::default())
}

/// A message whose raw bytes carry `attachments` as `text/plain` parts.
async fn message_with_attachments(fixture: &Fixture, attachments: &[(&str, &str)]) -> i64 {
    let mut raw = String::from(
        "From: billing@acme.example.com\r\n\
Subject: Your invoice\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain\r\n\
\r\n\
Covering note.\r\n",
    );
    for (name, body) in attachments {
        raw.push_str(&format!(
            "--b1\r\n\
Content-Type: text/plain; name=\"{name}\"\r\n\
Content-Disposition: attachment; filename=\"{name}\"\r\n\
\r\n\
{body}\r\n"
        ));
    }
    raw.push_str("--b1--\r\n");

    let uid = fixture.next_uid.get();
    fixture.next_uid.set(uid + 1);
    let (account_id, mailbox_id) = (fixture.account_id, fixture.mailbox_id);
    fixture
        .db
        .write(move |c| {
            repo::insert_message(
                c,
                &repo::NewMessage {
                    account_id,
                    mailbox_id,
                    uid,
                    uidvalidity: 1,
                    subject: Some("Your invoice".to_owned()),
                    raw: Some(raw.into_bytes()),
                    ..Default::default()
                },
            )
        })
        .await
        .expect("message")
}

const BILL: &str = "Invoice\r\nBill to: Grace\r\nInvoice Number: INV-9\r\nTotal: $5.00\r\n";

#[tokio::test]
async fn a_model_pass_on_a_daemon_with_no_provider_is_refused() {
    let fixture = Fixture::open().await;
    let message_id = message_with_attachments(&fixture, &[("invoice-9.txt", BILL)]).await;
    let cancel = tokio_util::sync::CancellationToken::new();

    let error = engine(&fixture.db)
        .invoice(message_id, Some("0"), true, &cancel)
        .await
        .expect_err("there is no model to pass to");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);

    // And nothing was stored: a refused request must not leave a partial
    // deterministic reading behind labelled as if the model had run.
    let rows = store::list_invoices(&fixture.db, &InvoiceFilter::default())
        .await
        .expect("list");
    assert!(rows.is_empty(), "{rows:?}");
}

#[tokio::test]
async fn a_structured_extraction_on_a_daemon_with_no_provider_is_a_precondition_failure() {
    let fixture = Fixture::open().await;
    let message_id = message_with_attachments(&fixture, &[("invoice-9.txt", BILL)]).await;
    let cancel = tokio_util::sync::CancellationToken::new();

    let error = engine(&fixture.db)
        .structured(message_id, "invoice", None, false, &cancel)
        .await
        .expect_err("there is no provider");
    assert_eq!(error.reason(), ErrorReason::FailedPrecondition);

    // An unknown schema is the caller's error and is reported as one, even on
    // this daemon — the schema is checked before the provider is looked for.
    let error = engine(&fixture.db)
        .structured(message_id, "horoscope", None, false, &cancel)
        .await
        .expect_err("unknown schema");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn detection_opens_at_most_the_candidate_bound_and_prefers_a_named_file() {
    let fixture = Fixture::open().await;
    // More decoys than the bound, then the real bill last. Its filename is the
    // only thing that gets it looked at at all.
    let mut attachments: Vec<(String, String)> = (0..(MAX_LINE_ITEMS.min(8)))
        .map(|n| {
            (
                format!("notes-{n}.txt"),
                "just prose, no figures\r\n".to_owned(),
            )
        })
        .collect();
    attachments.push(("invoice-9.txt".to_owned(), BILL.to_owned()));
    let borrowed: Vec<(&str, &str)> = attachments
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_str()))
        .collect();
    let message_id = message_with_attachments(&fixture, &borrowed).await;
    let cancel = tokio_util::sync::CancellationToken::new();

    let report = engine(&fixture.db)
        .invoice(message_id, None, false, &cancel)
        .await
        .expect("the named bill is found first");
    assert_eq!(report.stored.invoice.part, "8", "the invoice-named file");
    assert_eq!(
        report.stored.invoice.total.as_ref().expect("total").value,
        Money {
            currency: "USD".to_owned(),
            minor_units: 500,
        }
    );
    assert!(!report.used_model);
    // The bound is what is being asserted: the decoys are not all opened.
    assert!(
        report.candidates.len() <= crate::extract::MAX_CANDIDATE_PARTS,
        "{:?}",
        report.candidates
    );
}

#[tokio::test]
async fn a_message_whose_attachments_are_all_prose_is_a_precondition_failure() {
    let fixture = Fixture::open().await;
    let message_id =
        message_with_attachments(&fixture, &[("notes.txt", "just prose, no figures\r\n")]).await;
    let cancel = tokio_util::sync::CancellationToken::new();

    let error = engine(&fixture.db)
        .invoice(message_id, None, false, &cancel)
        .await
        .expect_err("nothing here is a bill");
    assert_eq!(error.reason(), ErrorReason::FailedPrecondition);
}

#[tokio::test]
async fn a_part_the_detector_rejects_is_still_read_when_it_is_named_and_says_so() {
    // An explicit request is stronger than the detector's opinion, but the
    // disagreement has to be on the record.
    let fixture = Fixture::open().await;
    let message_id =
        message_with_attachments(&fixture, &[("notes.txt", "Widgets\r\nTotal: $5.00\r\n")]).await;
    let cancel = tokio_util::sync::CancellationToken::new();

    let report = engine(&fixture.db)
        .invoice(message_id, Some("0"), false, &cancel)
        .await
        .expect("named parts are read");
    assert!(
        report
            .stored
            .invoice
            .warnings
            .iter()
            .any(|warning| warning.contains("does not read as an invoice")),
        "{:?}",
        report.stored.invoice.warnings
    );
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].kind, None);
}

#[test]
fn money_past_the_detect_window_does_not_make_a_document_a_bill() {
    // The detector reads the head of a document, so a bill whose only figure
    // is past MAX_DETECT_BYTES is not claimed — and the same document with the
    // figure at the top is.
    let padding = "invoice amount due bill to payment terms\n".repeat(400);
    assert!(padding.len() > MAX_DETECT_BYTES);
    assert_eq!(
        detect(None, None, &format!("{padding}Total: $10.00\n")),
        None
    );
    assert_eq!(
        detect(None, None, &format!("Total: $10.00\n{padding}")),
        Some(DocKind::Invoice)
    );
}
