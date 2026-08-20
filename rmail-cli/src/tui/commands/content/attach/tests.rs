//! The attachment verbs: what is inside a document, and searching it.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_proto::v1::{
    AttachmentHit, ExtractInvoiceResponse, ExtractTablesResponse, ExtractedInvoice, FieldOrigin,
    FieldProvenance, InvoiceMoney, InvoiceText, SearchAttachmentsResponse, Table, TableColumn,
    TableRow,
};

use super::super::tests::{no_account, no_message, screen};
use super::*;
use crate::tui::commands::content::analytics::InvoiceFormat;
use crate::tui::model::wire;
use crate::tui::report::ReportTone;

fn invocation(line: &str) -> Invocation {
    match command::parse(line) {
        Ok(Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
    }
}

fn asked(line: &str, target: &Target) -> Answer {
    match answer(&invocation(line), target, 5) {
        Some(answer) => answer,
        None => panic!("{line:?} has no answer"),
    }
}

fn request(line: &str) -> Request {
    match asked(line, &screen()) {
        Answer::Rows(request) | Answer::Fact(request) => *request,
        other => panic!("{line:?} is not a request: {other:?}"),
    }
}

fn refusal(line: &str, target: &Target) -> String {
    match asked(line, target) {
        Answer::Refused(why) => why,
        other => panic!("{line:?} was not refused: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// the model is opt-in everywhere
// ---------------------------------------------------------------------------

#[test]
fn reading_what_a_parser_cannot_costs_money_and_is_asked_for() {
    let Cmd::AttachTables { allow_model, .. } = request("attach tables").cmd else {
        panic!("expected a table extraction");
    };
    assert!(!allow_model);
    let Cmd::AttachTables { allow_model, .. } = request("attach tables --model").cmd else {
        panic!("expected a table extraction");
    };
    assert!(allow_model);
    let Cmd::AttachInvoice { use_model, .. } = request("attach invoice --model").cmd else {
        panic!("expected an invoice extraction");
    };
    assert!(use_model);
}

#[test]
fn a_part_can_be_named_and_the_message_defaults_to_the_open_one() {
    let Cmd::AttachTables {
        message_id, part, ..
    } = request("attach tables --part=2").cmd
    else {
        panic!("expected a table extraction");
    };
    assert_eq!(message_id, 10);
    assert_eq!(part.as_deref(), Some("2"));
    let Cmd::AttachTables { message_id, .. } = request("attach tables 42").cmd else {
        panic!("expected a table extraction");
    };
    assert_eq!(message_id, 42);
    assert!(refusal("attach tables", &no_message()).contains("no message selected"));
}

// ---------------------------------------------------------------------------
// what the rows say
// ---------------------------------------------------------------------------

#[test]
fn an_inferred_or_truncated_table_says_so() {
    // A spreadsheet silently short of three columns is a spreadsheet nobody can
    // trust, and a table a model invented is a guess about somebody's numbers.
    let response = ExtractTablesResponse {
        tables: vec![Table {
            name: "Q3".to_owned(),
            columns: vec![
                TableColumn {
                    header: "item".to_owned(),
                    r#type: 2,
                },
                TableColumn {
                    header: "total".to_owned(),
                    r#type: 3,
                },
            ],
            rows: vec![TableRow {
                cells: vec![
                    rmail_proto::v1::TableCell {
                        text: "widget".to_owned(),
                        r#type: 2,
                        ..Default::default()
                    },
                    rmail_proto::v1::TableCell {
                        text: String::new(),
                        r#type: 3,
                        number: 12.5,
                        ..Default::default()
                    },
                ],
            }],
            origin: 4,
            inferred: true,
            truncated: true,
        }],
        dropped_tables: 2,
        cell_budget_exhausted: true,
    };
    let rows = wire::table_rows(&response);
    // The header row names the table once, then the data.
    assert_eq!(rows[0].cells[0], "Q3");
    assert_eq!(rows[0].cells[1], "item");
    assert_eq!(rows[1].cells[2], "12.5", "a number cell renders its number");
    assert!(rows.iter().any(|row| row.cells[1].contains("truncated")));
    assert!(rows
        .iter()
        .any(|row| row.cells[1].contains("inferred by a model")));
    let dropped = rows
        .iter()
        .find(|row| row.cells[0] == "dropped")
        .expect("a dropped row");
    assert_eq!(dropped.tone, ReportTone::Warn);
}

#[test]
fn an_invoice_field_says_whether_it_was_parsed_or_guessed() {
    // A total a parser read out of a text layer and a total a model inferred from
    // a scan are not the same claim, and a report that flattened them would be
    // inviting somebody to pay the second one.
    let response = ExtractInvoiceResponse {
        invoice: Some(ExtractedInvoice {
            invoice_id: 1,
            message_id: 10,
            part_id: "2".to_owned(),
            kind: 1,
            vendor: Some(InvoiceText {
                value: "Acme".to_owned(),
                provenance: Some(FieldProvenance {
                    origin: FieldOrigin::Parsed as i32,
                    ..Default::default()
                }),
            }),
            number: None,
            currency: "USD".to_owned(),
            subtotal: None,
            tax: None,
            total: Some(InvoiceMoney {
                currency: "USD".to_owned(),
                minor_units: 12_345,
                provenance: Some(FieldProvenance {
                    origin: FieldOrigin::Model as i32,
                    ..Default::default()
                }),
            }),
            issued_at: None,
            due_at: None,
            status: 3,
            status_provenance: None,
            line_items: Vec::new(),
            warnings: vec!["two candidate totals".to_owned()],
            inferred: true,
            extracted_at: 0,
        }),
        candidates: Vec::new(),
        used_model: true,
    };
    let rows = wire::invoice_rows(&response);
    let cell = |what: &str| {
        rows.iter()
            .find(|row| row.cells[0] == what)
            .cloned()
            .unwrap_or_else(|| panic!("no {what} row"))
    };
    assert_eq!(cell("vendor").cells[2], "parsed");
    assert_eq!(cell("total").cells[1], "USD 123.45");
    assert_eq!(cell("total").cells[2], "model");
    // Overdue is the one status worth spotting.
    assert_eq!(cell("status").cells[1], "overdue");
    assert_eq!(cell("status").tone, ReportTone::Bad);
    assert!(rows.iter().any(|row| row.cells[0] == "inferred"));
    assert!(rows.iter().any(|row| row.cells[0] == "warning"));
}

#[test]
fn a_message_with_no_invoice_lists_what_it_did_find() {
    let response = ExtractInvoiceResponse {
        invoice: None,
        candidates: vec![rmail_proto::v1::InvoiceCandidate {
            part_id: "3".to_owned(),
            filename: "scan.pdf".to_owned(),
            kind: 0,
        }],
        used_model: false,
    };
    let rows = wire::invoice_rows(&response);
    assert_eq!(rows[0].cells[0], "nothing");
    assert_eq!(rows[1].cells[1], "scan.pdf");
}

// ---------------------------------------------------------------------------
// ask and search
// ---------------------------------------------------------------------------

#[test]
fn asking_a_document_is_scoped_to_it_unless_the_account_is_asked_for() {
    // Retrieval across every attachment in an account is a much larger model
    // call, and somebody looking at a document usually means that document.
    let Cmd::AttachAsk {
        message_id,
        account_id,
        question,
        ..
    } = request("attach ask what is the total").cmd
    else {
        panic!("expected a question");
    };
    assert_eq!(message_id, 10);
    assert_eq!(account_id, 0, "one message needs no account filter");
    assert_eq!(question, "what is the total");

    let Cmd::AttachAsk {
        message_id,
        account_id,
        ..
    } = request("attach ask what did acme bill --all").cmd
    else {
        panic!("expected a question");
    };
    assert_eq!(message_id, 0, "zero is the whole account on this RPC");
    assert_eq!(account_id, 7);

    assert!(refusal("attach ask", &screen()).contains("ask something"));
    // Scoped to a message it does not have: refused, because the question was
    // about a document.
    assert!(refusal("attach ask what", &no_message()).contains("no message selected"));
    // Account-wide with no account: refused for the other reason.
    assert!(refusal("attach ask what --all", &no_account()).contains("no account"));
}

#[test]
fn searching_attachments_falls_back_to_the_account_rather_than_refusing() {
    // Zero is "the whole account" on this RPC, and with no message on screen that
    // is the useful reading of an unscoped search — unlike `:attach ask`, where
    // the question is usually about the document in front of somebody.
    let cmd = match asked("attach search invoice", &no_message()) {
        Answer::Rows(request) => request.cmd,
        other => panic!("{other:?}"),
    };
    let Cmd::AttachSearch { message_id, .. } = cmd else {
        panic!("expected a search");
    };
    assert_eq!(message_id, 0);
    assert!(refusal("attach search", &screen()).contains("search for what"));
}

#[test]
fn the_two_paths_to_searching_attachments_are_the_same_verb() {
    // `:attach search` and `:search attachments` belong to two families at once,
    // the way `:helpgrep` and `:manual grep` do. Different spelling, identical
    // request.
    let Cmd::AttachSearch { query, limit, .. } = request("attach search invoice --limit=5").cmd
    else {
        panic!("expected a search");
    };
    let Cmd::AttachSearch {
        query: other,
        limit: other_limit,
        ..
    } = request("search attachments invoice --limit=5").cmd
    else {
        panic!("expected a search");
    };
    assert_eq!(query, other);
    assert_eq!(limit, other_limit);
    assert_eq!(
        request("attach search x").columns,
        request("search attachments x").columns
    );
}

#[test]
fn an_attachment_hit_opens_the_message_it_is_in() {
    let response = SearchAttachmentsResponse {
        hits: vec![AttachmentHit {
            message_id: 42,
            message_uid: 0,
            account_id: 7,
            mailbox: "INBOX".to_owned(),
            subject: String::new(),
            from_addr: "ada@example.com".to_owned(),
            date: None,
            part_id: "2".to_owned(),
            filename: "invoice.pdf".to_owned(),
            content_type: "application/pdf".to_owned(),
            bytes: None,
            pages: None,
            page: Some(3),
            span_start: 0,
            span_end: 0,
            excerpt: "total due".to_owned(),
            provenance: String::new(),
            score: 0.9,
            lexical_rank: None,
            dense_rank: None,
        }],
    };
    let rows = wire::attachment_hit_rows(&response);
    assert_eq!(rows[0].cells[0], "invoice.pdf");
    // An empty subject reads as the message, not as a rendering fault.
    assert_eq!(rows[0].cells[2], "(no subject)");
    assert_eq!(rows[0].cells[3], "page 3");
    let opens = rows[0].on_enter.clone().expect("it opens the message");
    assert_eq!(opens.positionals, vec!["42".to_owned()]);
}

#[test]
fn the_invoice_export_takes_rows_or_a_document() {
    let Cmd::AttachInvoices { format, .. } = request("attach invoices").cmd else {
        panic!("expected an export");
    };
    assert_eq!(format, InvoiceFormat::Rows);
    let Cmd::AttachInvoices { format, .. } = request("attach invoices --format=csv").cmd else {
        panic!("expected an export");
    };
    assert_eq!(format, InvoiceFormat::Csv);
    let why = refusal("attach invoices --format=xlsx", &screen());
    assert!(why.contains("rows or csv"), "{why}");
}
