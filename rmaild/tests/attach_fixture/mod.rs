//! Fixtures shared by `attach_search.rs` and `ask_attachment.rs`: a
//! multipart RFC822 message and a genuine multi-page PDF.
//!
//! Built rather than checked in, and duplicated from `rmail-core`'s own
//! `attach::extract::tests` rather than exported from it. Both halves of that
//! are deliberate: a byte array in a test file is unreadable and
//! unmaintainable, and making a test fixture reachable from outside
//! `#[cfg(test)]` would put a PDF writer in the shipped library to save a
//! copy in a test directory.
//!
//! The PDF is hand-assembled for the reason the original gives: every PDF
//! library that writes one also reads one, so a fixture produced by the
//! library under test proves only that it agrees with itself.
#![allow(dead_code)] // each test binary uses a subset

/// A multipart message carrying the given attachments, plus a plain-text
/// body — the shape real mail with attachments actually has.
pub fn message_with(attachments: &[(&str, &str, &[u8])]) -> Vec<u8> {
    use std::fmt::Write;
    let mut out = String::from(
        "From: ada@example.com\r\n\
         To: bob@example.com\r\n\
         Subject: With attachments\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n\r\n\
         --BOUND\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n\
         Please see the attached.\r\n",
    );
    for (filename, content_type, bytes) in attachments {
        let encoded = base64(bytes);
        let _ = write!(
            out,
            "--BOUND\r\n\
             Content-Type: {content_type}\r\n\
             Content-Disposition: attachment; filename=\"{filename}\"\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n\
             {encoded}\r\n"
        );
    }
    out.push_str("--BOUND--\r\n");
    out.into_bytes()
}

/// Base64, wrapped, because a mail transfer encoding is part of the fixture.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for (n, block) in bytes.chunks(3).enumerate() {
        if n > 0 && n % 19 == 0 {
            out.push_str("\r\n");
        }
        let b = [
            *block.first().unwrap_or(&0),
            *block.get(1).unwrap_or(&0),
            *block.get(2).unwrap_or(&0),
        ];
        let triple = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= block.len() {
                let index = ((triple >> (18 - i * 6)) & 0x3f) as usize;
                out.push(char::from(ALPHABET[index]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A minimal but genuine PDF with one line of `text` per page.
pub fn pdf_bytes(pages: &[&str]) -> Vec<u8> {
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
