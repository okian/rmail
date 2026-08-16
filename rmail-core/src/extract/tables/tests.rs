//! Table extraction: the native grids, the typing rules, and every bound.
//!
//! The bounds tests are the point of this file. A table extractor that works on
//! a real invoice is easy; one that cannot be made to allocate a gigabyte or
//! spin a core from a 4 KB attachment is the thing being verified here, so
//! every named constant in the parent module has a test that actually reaches
//! it.

use std::io::Write;

use super::*;
use crate::ErrorReason;

// ---------------------------------------------------------------------------
// A minimal XLSX, built here so the spreadsheet route is tested for real
// ---------------------------------------------------------------------------

/// Build a one-sheet workbook from `rows` of `(cell ref, type attr, value)`.
///
/// Inline strings rather than a shared-string table: the point is to exercise
/// `calamine` and this module's grid assembly, not to reimplement the whole of
/// OOXML.
fn workbook(sheet: &str, cells: &[(&str, &str)]) -> Vec<u8> {
    let mut rows: std::collections::BTreeMap<u32, Vec<(&str, &str)>> =
        std::collections::BTreeMap::new();
    for (reference, value) in cells {
        let row: u32 = reference
            .trim_start_matches(|c: char| c.is_ascii_alphabetic())
            .parse()
            .expect("a1 row");
        rows.entry(row).or_default().push((reference, value));
    }
    let mut sheet_xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#,
    );
    for (row, cells) in rows {
        sheet_xml.push_str(&format!(r#"<row r="{row}">"#));
        for (reference, value) in cells {
            // A value that parses as a number is written as one, so the
            // workbook carries real types rather than text everywhere.
            if value.parse::<f64>().is_ok() {
                sheet_xml.push_str(&format!(r#"<c r="{reference}"><v>{value}</v></c>"#));
            } else {
                sheet_xml.push_str(&format!(
                    r#"<c r="{reference}" t="inlineStr"><is><t>{value}</t></is></c>"#
                ));
            }
        }
        sheet_xml.push_str("</row>");
    }
    sheet_xml.push_str("</sheetData></worksheet>");

    let mut buffer = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let files: [(&str, String); 5] = [
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#.to_owned(),
            ),
            (
                "_rels/.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_owned(),
            ),
            (
                "xl/workbook.xml",
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="{sheet}" sheetId="1" r:id="rId1"/></sheets></workbook>"#
                ),
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.to_owned(),
            ),
            ("xl/worksheets/sheet1.xml", sheet_xml),
        ];
        for (name, body) in files {
            zip.start_file(name, options).expect("zip entry");
            zip.write_all(body.as_bytes()).expect("zip write");
        }
        zip.finish().expect("zip finish");
    }
    buffer
}

fn cell(table: &Table, row: usize, col: usize) -> &Cell {
    table
        .rows
        .get(row)
        .and_then(|row| row.get(col))
        .expect("cell in range")
}

// ---------------------------------------------------------------------------
// Native: spreadsheets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_workbook_becomes_typed_rows_with_a1_provenance() {
    let bytes = workbook(
        "Q3 Forecast",
        &[
            ("A1", "Item"),
            ("B1", "Amount"),
            ("A2", "Widgets"),
            ("B2", "1299"),
            ("A3", "Gadgets"),
            ("B3", "42.5"),
        ],
    );
    let report = from_xlsx(bytes).await.expect("workbook parses");
    let table = report.tables.first().expect("one table");

    assert_eq!(
        table.name, "Q3 Forecast",
        "the sheet name is the table name"
    );
    assert_eq!(
        table
            .columns
            .iter()
            .map(|c| c.header.as_str())
            .collect::<Vec<_>>(),
        vec!["Item", "Amount"],
        "the header row is detected, not returned as data"
    );
    assert_eq!(table.columns[1].kind, CellType::Number);
    assert_eq!(table.rows.len(), 2, "the header is not a data row");
    assert_eq!(cell(table, 0, 1).value, CellValue::Number(1299.0));
    assert_eq!(
        cell(table, 0, 1).source.reference,
        "B2",
        "provenance names a cell an operator can type into the original"
    );
    assert_eq!(table.origin, TableOrigin::Spreadsheet);
    assert!(!table.inferred(), "a workbook cell is read, not inferred");
}

#[tokio::test]
async fn bytes_that_are_not_a_workbook_are_invalid_argument_not_a_panic() {
    let error = from_xlsx(b"PK\x03\x04 not really a workbook".to_vec())
        .await
        .expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn a_sheet_with_no_cells_yields_no_table() {
    let bytes = workbook("Empty", &[]);
    let report = from_xlsx(bytes).await.expect("workbook parses");
    assert!(report.tables.is_empty(), "an empty sheet is not a table");
}

#[test]
fn a1_references_are_correct_past_the_first_twenty_six_columns() {
    assert_eq!(a1(0, 0), "A1");
    assert_eq!(a1(6, 1), "B7");
    assert_eq!(a1(0, 25), "Z1");
    assert_eq!(a1(0, 26), "AA1");
    assert_eq!(a1(0, 51), "AZ1");
    assert_eq!(a1(0, 52), "BA1");
    assert_eq!(a1(0, 701), "ZZ1");
    assert_eq!(a1(0, 702), "AAA1");
}

// ---------------------------------------------------------------------------
// Native: delimited text
// ---------------------------------------------------------------------------

#[test]
fn a_csv_becomes_a_typed_table_with_a_detected_header() {
    let report = from_csv(
        "invoice.csv",
        "Item,Qty,Price\nWidget,3,19.99\nGadget,1,5\n",
    )
    .expect("csv parses");
    let table = report.tables.first().expect("one table");
    assert_eq!(
        table
            .columns
            .iter()
            .map(|c| c.header.as_str())
            .collect::<Vec<_>>(),
        vec!["Item", "Qty", "Price"]
    );
    assert_eq!(table.columns[1].kind, CellType::Number);
    assert_eq!(table.rows.len(), 2);
    assert_eq!(cell(table, 0, 2).value, CellValue::Number(19.99));
    assert_eq!(table.origin, TableOrigin::Csv);
}

#[test]
fn a_semicolon_delimiter_is_sniffed_rather_than_assumed() {
    let report = from_csv("export.csv", "Name;Total\nAda;12\nGrace;7\n").expect("csv parses");
    let table = report.tables.first().expect("one table");
    assert_eq!(
        table.columns.len(),
        2,
        "a comma parse would give one column"
    );
    assert_eq!(cell(table, 0, 1).value, CellValue::Number(12.0));
}

#[test]
fn quoting_survives_embedded_delimiters_newlines_and_doubled_quotes() {
    let report = from_csv(
        "q.csv",
        "Item,Note,N\n\"Widget, large\",\"line one\nline two\",4\n\"He said \"\"hi\"\"\",plain,5\n",
    )
    .expect("csv parses");
    let table = report.tables.first().expect("one table");
    assert_eq!(cell(table, 0, 0).text, "Widget, large");
    assert_eq!(cell(table, 0, 1).text, "line one\nline two");
    assert_eq!(cell(table, 1, 0).text, "He said \"hi\"");
    assert_eq!(
        table.rows.len(),
        2,
        "an embedded newline is not a new record"
    );
}

#[test]
fn a_ragged_record_is_padded_to_the_table_width() {
    let report = from_csv("r.csv", "A,B,C\n1,2,3\n4\n").expect("csv parses");
    let table = report.tables.first().expect("one table");
    assert_eq!(table.rows[1].len(), 3, "every row is indexable by column");
    assert_eq!(cell(table, 1, 2).value, CellValue::Empty);
}

#[test]
fn a_column_whose_cells_disagree_collapses_to_text() {
    let report = from_csv("m.csv", "A,B\nx,1\ny,n/a\nz,3\n").expect("csv parses");
    let table = report.tables.first().expect("one table");
    assert_eq!(
        table.columns[1].kind,
        CellType::Text,
        "a consumer that summed this column would be wrong"
    );
}

#[test]
fn a_header_is_not_claimed_when_every_row_is_text() {
    let report = from_csv("names.csv", "Ada,Lovelace\nGrace,Hopper\n").expect("csv parses");
    let table = report.tables.first().expect("one table");
    assert!(
        table.columns.iter().all(|column| column.header.is_empty()),
        "no header was detectable"
    );
    assert_eq!(table.rows.len(), 2, "and no row was silently eaten");
}

#[test]
fn an_oversized_csv_is_declined_rather_than_read() {
    let text = "a,b\n".repeat(MAX_CSV_BYTES / 4 + 8);
    let error = from_csv("big.csv", &text).expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn the_row_cap_bounds_a_hostile_csv() {
    let mut text = String::from("A,B\n");
    for index in 0..(MAX_ROWS + 200) {
        text.push_str(&format!("{index},{index}\n"));
    }
    let report = from_csv("rows.csv", &text).expect("csv parses");
    let table = report.tables.first().expect("one table");
    assert!(table.rows.len() < MAX_ROWS + 200);
    assert!(table.rows.len() <= MAX_ROWS);
    assert!(table.truncated, "and it says so");
}

#[test]
fn the_column_cap_bounds_a_single_enormous_record() {
    let wide: String = (0..MAX_COLS * 4)
        .map(|index| index.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let report = from_csv("wide.csv", &format!("{wide}\n{wide}\n")).expect("csv parses");
    let table = report.tables.first().expect("one table");
    assert!(
        table.rows.iter().all(|row| row.len() <= MAX_COLS),
        "no row is wider than the cap"
    );
}

#[test]
fn the_cell_budget_stops_a_document_that_is_all_cells() {
    // Under the row and column caps individually, past the global cell budget
    // in aggregate — the case a per-table bound alone would miss.
    let row: String = (0..100)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut text = String::new();
    for _ in 0..(MAX_CELLS / 100 + 50) {
        text.push_str(&row);
        text.push('\n');
    }
    let report = from_csv("budget.csv", &text).expect("csv parses");
    assert!(
        report.cell_budget_exhausted,
        "the budget bound, and said so"
    );
    let cells: usize = report.tables.iter().map(Table::cell_count).sum();
    assert!(cells <= MAX_CELLS, "and nothing past it was materialized");
}

#[test]
fn a_cell_longer_than_the_cap_is_cut_rather_than_held() {
    let long = "x".repeat(MAX_CELL_CHARS * 3);
    let report = from_csv("long.csv", &format!("A\n{long}\n")).expect("csv parses");
    let table = report.tables.first().expect("one table");
    assert!(cell(table, 0, 0).text.chars().count() <= MAX_CELL_CHARS);
}

// ---------------------------------------------------------------------------
// Typing
// ---------------------------------------------------------------------------

#[test]
fn currency_thousands_percentages_and_accounting_negatives_are_numbers() {
    assert_eq!(typed("$1,299.00"), CellValue::Number(1299.0));
    assert_eq!(typed("£42"), CellValue::Number(42.0));
    assert_eq!(typed("(1,234.00)"), CellValue::Number(-1234.0));
    assert_eq!(typed("12.5%"), CellValue::Number(0.125));
}

#[test]
fn an_ambiguous_date_stays_text_because_there_is_no_right_answer() {
    assert_eq!(
        typed("01/02/2024"),
        CellValue::Text("01/02/2024".to_owned()),
        "the second of January or the first of February; the cell does not say"
    );
    assert!(matches!(typed("2024-01-15"), CellValue::Date(_)));
    assert!(matches!(typed("2024-01-15T09:30:00"), CellValue::Date(_)));
}

#[test]
fn booleans_and_blanks_are_their_own_types() {
    assert_eq!(typed("yes"), CellValue::Bool(true));
    assert_eq!(typed("FALSE"), CellValue::Bool(false));
    assert_eq!(typed("   "), CellValue::Empty);
}

#[test]
fn a_cell_cannot_carry_bidi_overrides_into_a_terminal() {
    // A cell that reorders what a reader sees is the spreadsheet version of a
    // spoofed link, and this table is printed by `mail attach tables`.
    let hostile = "total\u{202e}gnidaelsim\u{202c}";
    let report = from_csv("h.csv", &format!("A\n{hostile}\n")).expect("csv parses");
    let table = report.tables.first().expect("one table");
    let text = &cell(table, 0, 0).text;
    assert!(
        !text.contains('\u{202e}'),
        "the override is stripped: {text:?}"
    );
    assert!(!text.contains('\u{202c}'));
}

// ---------------------------------------------------------------------------
// Native: HTML
// ---------------------------------------------------------------------------

#[test]
fn an_html_table_becomes_a_typed_table_with_its_caption() {
    let html = r#"
        <table><caption>Line items</caption>
          <tr><th>Item</th><th>Amount</th></tr>
          <tr><td>Widget</td><td>19.99</td></tr>
          <tr><td>Gadget</td><td>5</td></tr>
        </table>"#;
    let report = from_html(html).expect("html parses");
    let table = report.tables.first().expect("one table");
    assert_eq!(table.name, "Line items");
    assert_eq!(
        table
            .columns
            .iter()
            .map(|c| c.header.as_str())
            .collect::<Vec<_>>(),
        vec!["Item", "Amount"]
    );
    assert_eq!(cell(table, 0, 1).value, CellValue::Number(19.99));
    assert_eq!(table.origin, TableOrigin::Html);
    assert!(
        cell(table, 0, 0).source.reference.is_empty(),
        "html has no cell addresses, and an invented one would be worse than none"
    );
}

#[test]
fn a_layout_wrapper_is_not_returned_as_a_table() {
    let html = r#"<table><tr><td>banner image goes here</td></tr></table>"#;
    let report = from_html(html).expect("html parses");
    assert!(
        report.tables.is_empty(),
        "mail lays out with tables; a 1x1 is furniture, not data"
    );
}

#[test]
fn a_nested_table_does_not_leak_its_cells_into_its_parent() {
    let html = r#"
        <table>
          <tr><th>Outer A</th><th>Outer B</th></tr>
          <tr><td>1</td><td>
            <table><tr><th>In A</th><th>In B</th></tr><tr><td>9</td><td>8</td></tr></table>
          </td></tr>
        </table>"#;
    let report = from_html(html).expect("html parses");
    let outer = report.tables.first().expect("outer table");
    assert_eq!(
        outer
            .columns
            .iter()
            .map(|c| c.header.as_str())
            .collect::<Vec<_>>(),
        vec!["Outer A", "Outer B"]
    );
    assert!(
        outer.rows.iter().flatten().all(|cell| cell.text != "9"),
        "the inner table's numbers are not the outer table's row"
    );
    let inner = report
        .tables
        .get(1)
        .expect("the inner table is read on its own");
    assert_eq!(
        inner
            .columns
            .iter()
            .map(|c| c.header.as_str())
            .collect::<Vec<_>>(),
        vec!["In A", "In B"]
    );
}

#[test]
fn nesting_past_the_depth_cap_terminates_rather_than_recursing() {
    // A thousand-deep document costs a thousand increments, not a thousand
    // stack frames — this test exists to fail by stack overflow if that ever
    // stops being true.
    let mut html = String::new();
    for _ in 0..1_000 {
        html.push_str("<table><tr><td>x</td><td>y</td>");
    }
    for _ in 0..1_000 {
        html.push_str("</td></tr></table>");
    }
    let report = from_html(&html).expect("html parses");
    assert!(
        report.tables.iter().all(|table| table.truncated),
        "the depth cap bound it"
    );
}

#[test]
fn an_unclosed_table_keeps_what_it_read_and_stops() {
    let html = "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr>";
    let report = from_html(html).expect("html parses");
    let table = report.tables.first().expect("one table");
    assert_eq!(table.rows.len(), 1);
    assert!(table.truncated);
}

#[test]
fn an_oversized_html_document_is_declined_rather_than_read() {
    let html = "<td>x</td>".repeat(MAX_HTML_BYTES / 10 + 16);
    let error = from_html(&html).expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn html_entities_in_cells_are_decoded() {
    let html =
        "<table><tr><th>A</th><th>B</th></tr><tr><td>Tom &amp; Jerry</td><td>5</td></tr></table>";
    let report = from_html(html).expect("html parses");
    let table = report.tables.first().expect("one table");
    assert_eq!(cell(table, 0, 0).text, "Tom & Jerry");
}

// ---------------------------------------------------------------------------
// The model route
// ---------------------------------------------------------------------------

fn model_answer(json: serde_json::Value) -> TableReport {
    from_model_answer(&json.to_string()).expect("a well-formed answer parses")
}

#[test]
fn a_model_table_is_marked_inferred_and_carries_its_page() {
    let report = model_answer(serde_json::json!({
        "tables": [{
            "name": "Charges",
            "page": 3,
            "headers": ["Description", "Amount"],
            "rows": [["Line rental", "12.00"], ["Data", "5.00"]],
        }],
    }));
    let table = report.tables.first().expect("one table");
    assert_eq!(table.origin, TableOrigin::Model);
    assert!(
        table.inferred(),
        "a reader must be able to tell a transcription from a parse"
    );
    assert_eq!(cell(table, 0, 0).source.page, Some(3));
    assert_eq!(
        table
            .columns
            .iter()
            .map(|c| c.header.as_str())
            .collect::<Vec<_>>(),
        vec!["Description", "Amount"]
    );
    assert_eq!(cell(table, 0, 1).value, CellValue::Number(12.0));
}

#[test]
fn a_declared_header_survives_a_body_that_is_entirely_text() {
    // The native routes infer a header from type disagreement; that inference
    // would demote this declared header into a data row, and the row count is
    // how the bug shows.
    let report = model_answer(serde_json::json!({
        "tables": [{
            "name": "People",
            "page": 0,
            "headers": ["First", "Last"],
            "rows": [["Ada", "Lovelace"], ["Grace", "Hopper"]],
        }],
    }));
    let table = report.tables.first().expect("one table");
    assert_eq!(
        table
            .columns
            .iter()
            .map(|c| c.header.as_str())
            .collect::<Vec<_>>(),
        vec!["First", "Last"]
    );
    assert_eq!(table.rows.len(), 2, "and no data row was eaten");
    assert_eq!(cell(table, 0, 0).text, "Ada");
}

#[test]
fn a_model_answer_cannot_exceed_the_table_cap() {
    // Tiny tables, so the cell budget cannot be what stops this: the table cap
    // has to bind on its own, and the excess has to be counted rather than
    // silently dropped.
    let tables: Vec<serde_json::Value> = (0..MAX_TABLES + 10)
        .map(|index| {
            serde_json::json!({
                "name": format!("T{index}"),
                "page": 0,
                "headers": ["A", "B"],
                "rows": [["1", "2"], ["3", "4"]],
            })
        })
        .collect();
    let report = from_model_answer(&serde_json::json!({"tables": tables}).to_string())
        .expect("answer parses");
    assert_eq!(report.tables.len(), MAX_TABLES);
    assert_eq!(report.dropped_tables, 10, "and the excess is counted");
    assert!(
        !report.cell_budget_exhausted,
        "the cell budget was not the bound"
    );
}

#[test]
fn a_model_answer_cannot_exceed_the_row_or_cell_caps() {
    let rows: Vec<Vec<String>> = (0..MAX_ROWS + 500)
        .map(|index| vec![index.to_string(), "x".to_owned()])
        .collect();
    let tables: Vec<serde_json::Value> = (0..8)
        .map(|index| {
            serde_json::json!({
                "name": format!("T{index}"),
                "page": 0,
                "headers": [],
                "rows": rows,
            })
        })
        .collect();
    let report = from_model_answer(&serde_json::json!({"tables": tables}).to_string())
        .expect("answer parses");
    assert!(
        report
            .tables
            .iter()
            .all(|table| table.rows.len() <= MAX_ROWS),
        "a cap is a guarantee, not an instruction the model was given"
    );
    let cells: usize = report.tables.iter().map(Table::cell_count).sum();
    assert!(cells <= MAX_CELLS, "{cells} cells materialized");
    assert!(report.cell_budget_exhausted, "and the budget says it bound");
}

#[test]
fn a_model_answer_that_is_not_the_requested_schema_is_an_internal_error() {
    let error = from_model_answer("not json at all").expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::Internal);
    let error = from_model_answer(r#"{"tables": [{"name": 7}]}"#).expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::Internal);
}

#[test]
fn a_model_cell_cannot_carry_bidi_overrides_into_a_terminal() {
    let report = model_answer(serde_json::json!({
        "tables": [{
            "name": "X",
            "page": 0,
            "headers": [],
            "rows": [["total\u{202e}gnidaelsim\u{202c}", "1"]],
        }],
    }));
    let table = report.tables.first().expect("one table");
    assert!(!cell(table, 0, 0).text.contains('\u{202e}'));
}

// ---------------------------------------------------------------------------
// The wire vocabularies
// ---------------------------------------------------------------------------

#[test]
fn every_cell_type_and_origin_round_trips_through_its_string_form() {
    for kind in CellType::ALL {
        assert_eq!(CellType::parse(kind.as_str()), Some(kind));
    }
    for origin in TableOrigin::ALL {
        assert_eq!(TableOrigin::parse(origin.as_str()), Some(origin));
    }
    assert_eq!(CellType::parse("something-new"), None);
    assert_eq!(TableOrigin::parse("vision"), None);
}
