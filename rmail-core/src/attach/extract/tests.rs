//! Per-format fixtures, and the guarantees that hold for hostile input.
//!
//! Fixtures are *built* rather than checked in: a byte array in a test file is
//! unreadable, unmaintainable, and — for the zip formats — impossible to review.
//! Building them means the test says what shape it is testing.

use super::*;

/// A minimal but genuine PDF with `text` on one page.
///
/// Hand-assembled because every PDF library that writes one also reads one, and
/// a fixture produced by the library under test proves only that it agrees with
/// itself.
pub(crate) fn pdf_bytes(pages: &[&str]) -> Vec<u8> {
    let mut objects: Vec<String> = Vec::new();
    let page_count = pages.len();
    // 1: catalog, 2: page tree, then per page: page object + content stream.
    objects.push("<< /Type /Catalog /Pages 2 0 R >>".to_owned());
    let kids: String = (0..page_count)
        .map(|n| format!("{} 0 R ", 3 + n * 2))
        .collect();
    objects.push(format!(
        "<< /Type /Pages /Kids [{kids}] /Count {page_count} >>"
    ));
    for (n, text) in pages.iter().enumerate() {
        let content_id = 4 + n * 2;
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 {} 0 R >> >> /Contents {content_id} 0 R >>",
            3 + page_count * 2
        ));
        let stream = format!("BT /F1 12 Tf 72 720 Td ({text}) Tj ET");
        objects.push(format!(
            "<< /Length {} >>\nstream\n{stream}\nendstream",
            stream.len()
        ));
    }
    objects.push(
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_owned(),
    );

    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (n, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{body}\nendobj\n", n + 1));
    }
    let xref_at = out.len();
    out.push_str(&format!("xref\n0 {}\n", objects.len() + 1));
    out.push_str("0000000000 65535 f \n");
    for offset in &offsets {
        out.push_str(&format!("{offset:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
        objects.len() + 1
    ));
    out.into_bytes()
}

/// A zip holding the named entries, which is what every OOXML format is.
fn zip_bytes(entries: &[(&str, &str)]) -> Vec<u8> {
    use std::io::Write;
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        for (name, body) in entries {
            writer
                .start_file::<_, ()>(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(body.as_bytes()).unwrap();
        }
        writer.finish().unwrap();
    }
    buffer.into_inner()
}

fn docx_bytes(paragraphs: &[&str]) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|text| format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>"))
        .collect();
    zip_bytes(&[
        ("[Content_Types].xml", "<Types/>"),
        (
            "word/document.xml",
            &format!(r#"<?xml version="1.0"?><w:document><w:body>{body}</w:body></w:document>"#),
        ),
    ])
}

fn pptx_bytes(slides: &[&str]) -> Vec<u8> {
    let mut entries: Vec<(String, String)> =
        vec![("ppt/presentation.xml".to_owned(), "<p/>".to_owned())];
    for (n, text) in slides.iter().enumerate() {
        entries.push((
            format!("ppt/slides/slide{}.xml", n + 1),
            format!(
                r#"<?xml version="1.0"?><p:sld><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:sld>"#
            ),
        ));
    }
    // Written in an order no reader should trust. A zip's entry order is
    // whatever the writer chose, and a fixture that happens to write them in
    // reading order cannot tell a sorted extractor from an unsorted one.
    entries.reverse();
    let borrowed: Vec<(&str, &str)> = entries
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    zip_bytes(&borrowed)
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

#[test]
fn magic_bytes_beat_a_wrong_declared_type() {
    // `application/octet-stream` is the commonest declared type for an
    // attachment of any format at all: whatever sent it guessed, and guessed
    // badly. Trusting the declaration first would route most real attachments
    // to `Unsupported`.
    let pdf = pdf_bytes(&["hello"]);
    assert_eq!(
        detect(&pdf, Some("report.bin"), Some("application/octet-stream")),
        Some(Format::Pdf)
    );
    assert_eq!(detect(&pdf, None, None), Some(Format::Pdf));
}

#[test]
fn an_ooxml_zip_is_identified_by_its_extension_then_by_its_parts() {
    let docx = docx_bytes(&["hello"]);
    assert_eq!(detect(&docx, Some("a.docx"), None), Some(Format::Docx));
    // No extension: the container's own parts say what it is.
    assert_eq!(
        detect(&docx, None, Some("application/octet-stream")),
        Some(Format::Docx)
    );
    let pptx = pptx_bytes(&["hello"]);
    assert_eq!(detect(&pptx, None, None), Some(Format::Pptx));
}

#[test]
fn the_container_decides_the_format_not_the_filename() {
    // The sender picked the filename most freely of all. An xlsx named `.docx`
    // routed to the DOCX extractor produces `Empty`, which is not retryable, so
    // it is silently unsearchable for the life of the mailbox.
    let xlsx = xlsx_fixture();
    assert_eq!(detect(&xlsx, Some("report.docx"), None), Some(Format::Xlsx));
    let docx = docx_bytes(&["hello"]);
    assert_eq!(detect(&docx, Some("report.xlsx"), None), Some(Format::Docx));
    let pptx = pptx_bytes(&["hello"]);
    assert_eq!(detect(&pptx, Some("report.docx"), None), Some(Format::Pptx));
}

#[tokio::test]
async fn a_spreadsheet_with_a_far_flung_cell_costs_what_its_cells_cost() {
    // `worksheet_range` builds a dense grid from the sheet's declared corner.
    // One cell at Z4000000, in a 1.4 KB attachment, allocated 3.3 GB in two
    // seconds — under every limit this module has, because the output is five
    // bytes. Rust aborts on allocation failure, so the panic guard cannot help.
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="4000000"><c r="Z4000000"><v>7</v></c></row>
</sheetData></worksheet>"#;
    let xlsx = xlsx_with_sheet(sheet);

    let started = std::time::Instant::now();
    let (status, text) = extract(Format::Xlsx, xlsx).await.unwrap();

    assert!(matches!(status, Status::Ok | Status::Empty), "{status:?}");
    assert!(text.text.len() < 1024, "{} bytes", text.text.len());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_spreadsheet_of_real_cells_is_bounded_by_the_cell_budget() {
    let mut rows = String::new();
    for r in 1..=6000 {
        rows.push_str(&format!("<row r=\"{r}\">"));
        for c in ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"] {
            rows.push_str(&format!("<c r=\"{c}{r}\" t=\"s\"><v>0</v></c>"));
        }
        rows.push_str("</row>");
    }
    let sheet = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows}</sheetData></worksheet>"#
    );
    let (status, text) = extract(Format::Xlsx, xlsx_with_sheet(&sheet))
        .await
        .unwrap();
    assert!(matches!(status, Status::Ok | Status::Empty));
    assert!(text.text.len() <= MAX_TEXT_BYTES);
}

#[test]
fn a_plain_zip_is_not_a_document() {
    let zip = zip_bytes(&[("notes.txt", "hello")]);
    assert_eq!(detect(&zip, Some("archive.zip"), None), None);
}

#[test]
fn an_unrecognized_attachment_is_not_guessed_at() {
    // Running arbitrary bytes through a text extractor produces a page of
    // mojibake that pollutes the index and matches queries at random. An
    // `Unsupported` row is a fact somebody can act on.
    assert_eq!(
        detect(
            &[0xff, 0xd8, 0xff, 0xe0, 0, 0],
            Some("photo.jpg"),
            Some("image/jpeg")
        ),
        None
    );
    assert_eq!(detect(b"\x7fELF\x02\x01", Some("a.out"), None), None);
}

#[test]
fn a_declared_text_type_is_the_last_resort() {
    assert_eq!(
        detect(b"a,b,c", Some("data"), Some("text/csv")),
        Some(Format::Csv)
    );
    assert_eq!(
        detect(b"hello", Some("data"), Some("text/plain; charset=utf-8")),
        Some(Format::Text)
    );
    assert_eq!(
        detect(b"<p>hi</p>", None, Some("text/html")),
        Some(Format::Html)
    );
}

// ---------------------------------------------------------------------------
// Per-format fixtures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_pdf_yields_its_text_and_its_page_boundaries() {
    let pdf = pdf_bytes(&["First page about invoices", "Second page about hosting"]);
    let (status, text) = extract(Format::Pdf, pdf).await.unwrap();

    assert_eq!(status, Status::Ok, "{text:?}");
    assert!(text.text.contains("invoices"), "{:?}", text.text);
    assert!(text.text.contains("hosting"), "{:?}", text.text);
    assert_eq!(text.pages.len(), 2, "a citation has to name the page");
    // Every span must be a valid slice of the text it describes, or the first
    // citation panics.
    for (start, end) in &text.pages {
        assert!(text.text.is_char_boundary(*start) && text.text.is_char_boundary(*end));
        assert!(*end <= text.text.len());
    }
    assert!(
        text.pages[0].1 <= text.pages[1].0,
        "pages must not overlap: {:?}",
        text.pages
    );
}

#[tokio::test]
async fn a_docx_yields_its_paragraphs_without_running_them_together() {
    // Two paragraphs concatenated with no separator produce a token that
    // appears in neither — "invoicehosting" is findable by nothing.
    let docx = docx_bytes(&["Quarterly invoice", "Hosting charges"]);
    let (status, text) = extract(Format::Docx, docx).await.unwrap();

    assert_eq!(status, Status::Ok);
    assert!(text.text.contains("Quarterly invoice"));
    assert!(text.text.contains("Hosting charges"));
    assert!(
        !text.text.contains("invoiceHosting"),
        "paragraphs ran together: {:?}",
        text.text
    );
}

#[tokio::test]
async fn a_pptx_reads_its_slides_in_order() {
    // Zip entry order is whatever the writer chose and lexical order puts slide
    // ten before slide two — which would make the same file extract to
    // different text on different days, and the content hash read that as a
    // change.
    let slides: Vec<String> = (1..=12).map(|n| format!("Slide{n} content")).collect();
    let borrowed: Vec<&str> = slides.iter().map(String::as_str).collect();
    let (status, text) = extract(Format::Pptx, pptx_bytes(&borrowed)).await.unwrap();

    assert_eq!(status, Status::Ok);
    let at = |needle: &str| text.text.find(needle);
    assert!(
        at("Slide2 content") < at("Slide10 content"),
        "{:?}",
        text.text
    );
    assert!(at("Slide1 content") < at("Slide2 content"));
}

#[tokio::test]
async fn an_xlsx_keeps_a_row_together_and_indexes_the_sheet_name() {
    // A row is a record: splitting it destroys the adjacency that makes
    // "invoice 4471" findable as a phrase. And "Q3 Forecast" is often the most
    // searchable thing in a workbook while appearing in no cell.
    let xlsx = xlsx_fixture();
    let (status, text) = extract(Format::Xlsx, xlsx).await.unwrap();

    assert_eq!(status, Status::Ok, "{text:?}");
    assert!(text.text.contains("Q3 Forecast"), "{:?}", text.text);
    assert!(
        text.text.contains("invoice\t4471") || text.text.contains("invoice 4471"),
        "the row did not stay together: {:?}",
        text.text
    );
}

#[tokio::test]
async fn html_and_plain_text_come_through_stripped_and_decoded() {
    let (status, html) = extract(Format::Html, b"<p>Hello <b>world</b></p>".to_vec())
        .await
        .unwrap();
    assert_eq!(status, Status::Ok);
    assert!(html.text.contains("Hello world"), "{:?}", html.text);
    assert!(!html.text.contains("<b>"));

    let (status, csv) = extract(Format::Csv, b"a,b\n1,2\n".to_vec()).await.unwrap();
    assert_eq!(status, Status::Ok);
    assert!(csv.text.contains('1'));
}

#[tokio::test]
async fn text_that_is_not_utf8_is_decoded_rather_than_mangled() {
    // A Windows-1252 CSV from an accounting package is an ordinary thing to
    // receive; mail attachments predate the consensus on UTF-8 by decades.
    let latin1 = b"Facturation r\xe9glement\xe9e".to_vec();
    let (status, text) = extract(Format::Text, latin1).await.unwrap();
    assert_eq!(status, Status::Ok);
    assert!(
        text.text.contains("réglementée"),
        "not decoded: {:?}",
        text.text
    );
}

// ---------------------------------------------------------------------------
// Hostile input
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_malformed_file_of_every_format_is_a_status_not_a_panic() {
    // An attachment is a file a stranger sent, and `pdf-extract` alone has
    // around a hundred panicking call sites. A panic here takes the daemon
    // down, and with it every account's mail.
    let corpus: Vec<Vec<u8>> = vec![
        b"%PDF-1.4\nnot actually a pdf at all".to_vec(),
        b"%PDF-".to_vec(),
        b"PK\x03\x04 truncated zip".to_vec(),
        vec![0u8; 512],
        vec![0xffu8; 512],
        b"<html><p>".to_vec(),
        Vec::new(),
    ];
    for format in [
        Format::Pdf,
        Format::Docx,
        Format::Xlsx,
        Format::Pptx,
        Format::Html,
        Format::Csv,
        Format::Text,
    ] {
        for bytes in &corpus {
            let (status, text) = extract(format, bytes.clone()).await.unwrap();
            assert!(
                Status::ALL.contains(&status),
                "{format:?} produced an impossible status"
            );
            assert!(text.text.len() <= MAX_TEXT_BYTES);
        }
    }
}

#[tokio::test]
async fn a_zip_bomb_does_not_get_to_declare_its_own_size() {
    // A zip declares its uncompressed size in the header and is under no
    // obligation to be telling the truth. The bound has to be on the read.
    //
    // The output cap below (`MAX_TEXT_BYTES`) does not prove that on its own:
    // an extractor that read all 13 MB and *then* truncated its result would
    // satisfy it while doing exactly the unbounded work this test exists to
    // rule out. So the read bound is measured by how cost responds to input
    // size. Doubling the archive must not double the time — past the cap the
    // extractor should stop, so the second one costs about what the first
    // does. An absolute wall-clock bound was tried first and is not viable
    // here: it measures the machine, and it failed at 20.3s against a 20s
    // limit purely because another container was building alongside it.
    // Each paragraph carries 200 characters, so 15,000 of them yield ~3 MB of
    // text — comfortably past `MAX_TEXT_BYTES` (2 MB), which is the whole
    // point. An earlier version of this test used one character per paragraph
    // and 400,000 paragraphs: ~800 KB, *under* the cap, so the cap never
    // engaged and the extractor was measured doing ordinary bounded work. Both
    // the timing assertion and `len() <= MAX_TEXT_BYTES` passed trivially
    // without either one ever exercising a bomb.
    let filler = "x".repeat(200);
    let build = |reps: usize| {
        let payload = format!("<w:p><w:r><w:t>{filler}</w:t></w:r></w:p>").repeat(reps);
        zip_bytes(&[(
            "word/document.xml",
            &format!("<w:document><w:body>{payload}</w:body></w:document>"),
        )])
    };
    let small = build(15_000);
    let double = build(30_000);

    let started = std::time::Instant::now();
    let (status, text) = extract(Format::Docx, small).await.unwrap();
    let small_elapsed = started.elapsed();

    let started = std::time::Instant::now();
    let (double_status, double_text) = extract(Format::Docx, double).await.unwrap();
    let double_elapsed = started.elapsed();

    assert!(matches!(status, Status::Ok | Status::Empty));
    assert!(matches!(double_status, Status::Ok | Status::Empty));
    // The cap must actually engage, or the rest of this test proves nothing.
    assert!(
        text.truncated,
        "the fixture no longer exceeds MAX_TEXT_BYTES ({} bytes produced), so \
         this test is not exercising the cap at all",
        text.text.len()
    );
    assert!(
        text.text.len() <= MAX_TEXT_BYTES,
        "produced {} bytes",
        text.text.len()
    );
    assert!(
        double_text.text.len() <= MAX_TEXT_BYTES,
        "twice the archive produced {} bytes",
        double_text.text.len()
    );
    // 1.6x leaves room for the extra inflate the bigger archive genuinely
    // costs, while still separating "stopped at the cap" from "read it all":
    // a read proportional to input would land at ~2x or beyond. Load moves
    // both measurements together, so the ratio holds on a busy machine.
    let ratio = double_elapsed.as_secs_f64() / small_elapsed.as_secs_f64().max(0.001);
    assert!(
        ratio < 1.6,
        "cost scaled with the archive rather than stopping at the cap: \
         {small_elapsed:?} for 15k paragraphs vs {double_elapsed:?} for 30k ({ratio:.2}x)"
    );
}

#[tokio::test]
async fn a_zip_with_an_absurd_number_of_entries_is_bounded() {
    let entries: Vec<(String, String)> = (0..20_000)
        .map(|n| (format!("word/document.xml{n}"), String::new()))
        .collect();
    let borrowed: Vec<(&str, &str)> = entries
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    // Deflating twenty thousand entries is most of the cost of this test, and
    // it is the harness's cost, not the extractor's. Timing it too meant a busy
    // machine failed a bound the extractor never came close to spending.
    let archive = zip_bytes(&borrowed);

    let started = std::time::Instant::now();
    let (status, _) = extract(Format::Docx, archive).await.unwrap();
    let elapsed = started.elapsed();

    assert!(matches!(status, Status::Empty | Status::Ok));
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "took {elapsed:?}"
    );
}

#[tokio::test]
async fn a_file_with_no_text_is_empty_rather_than_failed() {
    // A scanned PDF is a candidate for OCR; a broken extractor is a bug. The
    // pipeline retries one and not the other, so collapsing them means either
    // retrying scans for ever or never noticing a breakage.
    let (status, _) = extract(Format::Docx, docx_bytes(&[])).await.unwrap();
    assert_eq!(status, Status::Empty);

    let (status, _) = extract(Format::Text, b"   \n\t  ".to_vec()).await.unwrap();
    assert_eq!(status, Status::Empty);
}

#[tokio::test]
async fn an_extractor_that_panics_is_a_failed_status_and_nothing_more() {
    // `pdf-extract` alone has around a hundred panicking call sites, and an
    // attachment is a file a stranger sent. A panic reaching the runtime takes
    // the daemon down, and with it every account's mail — over one message.
    let (status, text) = isolate("probe", EXTRACT_TIMEOUT, || {
        #[expect(clippy::panic, reason = "reproducing the condition requires it")]
        {
            panic!("a malformed file did this");
        }
    })
    .await
    .unwrap();

    assert_eq!(status, Status::Failed);
    assert_eq!(text, Extracted::default());

    // And the runtime is still usable afterwards, which is the whole claim.
    let (status, _) = isolate("probe", EXTRACT_TIMEOUT, || {
        (Status::Ok, Extracted::default())
    })
    .await
    .unwrap();
    assert_eq!(status, Status::Ok);
}

#[tokio::test]
async fn an_extractor_that_never_finishes_is_abandoned() {
    // A PDF with a pathological content stream can occupy a core indefinitely,
    // and a blocking task cannot be cancelled. Abandoning the result is not a
    // real deadline, but it is what keeps one attachment from stalling every
    // attachment behind it.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    // A short deadline rather than the real one: the point is that the wait
    // ends, not how long it is, and a test that spends a minute proving it is a
    // test people start skipping.
    let deadline = std::time::Duration::from_millis(50);
    let started = std::time::Instant::now();
    let (status, _) = isolate("probe", deadline, move || {
        // Released by the test once the deadline has been observed, so the
        // thread is not left running for the life of the process.
        let _ = rx.recv();
        (Status::Ok, Extracted::default())
    })
    .await
    .unwrap();

    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(
        status,
        Status::Timeout,
        "the deadline must be enforced even though the work continues"
    );
    assert!(
        !Status::Timeout.is_retryable(),
        "a file that takes a minute takes a minute every time; a retryable \
         timeout is a job that re-arms itself for ever"
    );
    let _ = tx.send(());
}

#[test]
fn only_a_hard_failure_is_worth_another_attempt() {
    // Re-running an unsupported format or an oversized file changes nothing and
    // costs the same every time.
    assert!(Status::Failed.is_retryable());
    for status in [
        Status::Ok,
        Status::Empty,
        Status::TooLarge,
        Status::Unsupported,
        Status::Encrypted,
    ] {
        assert!(!status.is_retryable(), "{status:?}");
    }
}

#[test]
fn every_status_round_trips_through_storage() {
    for status in Status::ALL {
        assert_eq!(Status::parse(status.as_str()).unwrap(), status);
    }
    assert_eq!(
        Status::parse("nope").unwrap_err().reason(),
        crate::ErrorReason::Internal
    );
}

#[tokio::test]
async fn extraction_is_deterministic() {
    // The content hash is over the attachment's *bytes*, but the skip decision
    // is only useful if the same bytes produce the same text — otherwise a
    // re-index that skips leaves the index describing a different extraction
    // than the one recorded.
    let pdf = pdf_bytes(&["Invoice INV-9", "Hosting charges"]);
    let first = extract(Format::Pdf, pdf.clone()).await.unwrap();
    let second = extract(Format::Pdf, pdf).await.unwrap();
    assert_eq!(first, second);
}

#[test]
fn page_offsets_describe_the_text_that_is_actually_stored() {
    // The offsets have to point into the *normalized* text. Normalization
    // collapses whitespace, and whitespace is not evenly distributed across
    // pages — a sparsely set title page followed by dense body pages is what
    // most real documents look like. Scaling pre-normalization offsets by a
    // global length ratio put five of six marks on the wrong page here.
    let pages: Vec<String> = vec![
        // A title page: almost all whitespace, so normalization removes most
        // of it and the scale factor for this page is nothing like the rest.
        format!("PAGE1MARK{}", "\n".repeat(400)),
        format!("PAGE2MARK {}", "dense body text ".repeat(30)),
        format!("PAGE3MARK {}", "dense body text ".repeat(30)),
        format!("PAGE4MARK{}", " ".repeat(800)),
        format!("PAGE5MARK {}", "dense body text ".repeat(30)),
        format!("PAGE6MARK {}", "dense body text ".repeat(30)),
    ];
    let extracted = join_pages(pages);

    assert_eq!(extracted.pages.len(), 6);
    for (n, (start, end)) in extracted.pages.iter().enumerate() {
        assert!(
            extracted.text.is_char_boundary(*start) && extracted.text.is_char_boundary(*end),
            "page {} span {start}..{end} is not a valid slice",
            n + 1
        );
        assert!(start <= end && *end <= extracted.text.len());
        let mark = format!("PAGE{}MARK", n + 1);
        let at = extracted.text.find(&mark).expect("the mark survived");
        assert!(
            at >= *start && at < *end,
            "PAGE{}MARK is at {at}, outside its own page's span {start}..{end}",
            n + 1
        );
    }
    // And the spans partition the text: no overlaps, in order.
    for pair in extracted.pages.windows(2) {
        assert!(
            pair[0].1 <= pair[1].0,
            "pages overlap: {:?}",
            extracted.pages
        );
    }
}

#[test]
fn truncation_stops_at_a_whole_page_and_says_so() {
    // A span pointing past the end of the string it describes is a panic in the
    // citation renderer, and truncation is exactly where one would come from.
    let pages: Vec<String> = (0..40)
        .map(|n| format!("page {n} {}", "word ".repeat(MAX_TEXT_BYTES / 40)))
        .collect();
    let extracted = join_pages(pages);

    assert!(extracted.truncated);
    assert!(extracted.text.len() <= MAX_TEXT_BYTES);
    for (start, end) in &extracted.pages {
        assert!(*end <= extracted.text.len(), "{start}..{end}");
        assert!(extracted.text.is_char_boundary(*start));
        assert!(extracted.text.is_char_boundary(*end));
    }
}

#[test]
fn natural_order_puts_slide_two_before_slide_ten() {
    let mut names = vec![
        "ppt/slides/slide10.xml".to_owned(),
        "ppt/slides/slide2.xml".to_owned(),
        "ppt/slides/slide1.xml".to_owned(),
    ];
    names.sort_by(|a, b| natural_order(a, b));
    assert_eq!(
        names,
        vec![
            "ppt/slides/slide1.xml".to_owned(),
            "ppt/slides/slide2.xml".to_owned(),
            "ppt/slides/slide10.xml".to_owned(),
        ]
    );
}

/// A real XLSX, written by `rust_xlsxwriter`-free hand assembly.
///
/// Two sheets' worth of machinery is more than this needs; one sheet with a
/// shared-string table and an inline number exercises the paths that matter.
fn xlsx_with_sheet(sheet: &str) -> Vec<u8> {
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
 xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
    let rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
    let root_rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
    let shared = r#"<?xml version="1.0"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1">
<si><t>cell</t></si></sst>"#;
    zip_bytes(&[
        ("[Content_Types].xml", "<Types/>"),
        ("_rels/.rels", root_rels),
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", rels),
        ("xl/sharedStrings.xml", shared),
        ("xl/worksheets/sheet1.xml", sheet),
    ])
}

fn xlsx_fixture() -> Vec<u8> {
    let shared = r#"<?xml version="1.0"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="2" uniqueCount="2">
<si><t>invoice</t></si><si><t>hosting</t></si></sst>"#;
    let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1"><v>4471</v></c></row>
<row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2"><v>1299</v></c></row>
</sheetData></worksheet>"#;
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
 xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Q3 Forecast" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
    let rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
    let content_types = r#"<?xml version="1.0"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
    zip_bytes(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", rels),
        ("xl/sharedStrings.xml", shared),
        ("xl/worksheets/sheet1.xml", sheet),
    ])
}
