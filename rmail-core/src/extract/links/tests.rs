//! Link extraction: identity, the phishing case, the bounds, and what a model
//! is not allowed to do to the picker.
//!
//! The tests that matter most here are the ones about *disagreement*: a link
//! whose text says one host and whose target is another, and a model answer
//! that tries to talk the picker into floating it anyway. Both are the shape of
//! a real attack on a mail client, and neither is caught by a test that only
//! checks that URLs come out.

use super::*;
use crate::ErrorReason;

fn html(text: &str) -> Vec<LinkPart> {
    vec![LinkPart {
        part: "html:0".to_owned(),
        text: text.to_owned(),
        html: true,
    }]
}

fn plain(text: &str) -> Vec<LinkPart> {
    vec![LinkPart {
        part: "text:0".to_owned(),
        text: text.to_owned(),
        html: false,
    }]
}

fn find<'a>(report: &'a LinkReport, needle: &str) -> &'a Link {
    let urls: Vec<&str> = report.links.iter().map(|link| link.url.as_str()).collect();
    report
        .links
        .iter()
        .find(|link| link.url.contains(needle))
        .unwrap_or_else(|| unreachable!("no link containing {needle:?} in {urls:?}"))
}

// ---------------------------------------------------------------------------
// Extraction and identity
// ---------------------------------------------------------------------------

#[test]
fn a_bare_url_in_text_is_found_with_the_span_it_sits_at() {
    let body = "Docs are at https://example.com/guide, see you there.";
    let report = extract_links(&plain(body), &[]);
    let link = find(&report, "example.com/guide");
    assert_eq!(link.url, "https://example.com/guide");
    assert_eq!(link.display_text, "", "plain text has no anchor text");
    assert_eq!(
        &body[link.source.span_start..link.source.span_end],
        "https://example.com/guide",
        "the span names the bytes the URL actually occupies"
    );
}

#[test]
fn an_anchor_keeps_its_target_and_its_text_apart() {
    let report = extract_links(
        &html(r#"<a href="https://example.com/a">Read the report</a>"#),
        &[],
    );
    let link = find(&report, "example.com/a");
    assert_eq!(link.url, "https://example.com/a");
    assert_eq!(link.display_text, "Read the report");
    assert!(!link.deceptive, "prose text claims no host");
    assert_eq!(link.display_host, None);
}

#[test]
fn inner_markup_does_not_become_part_of_the_display_text() {
    let report = extract_links(
        &html(r#"<a href="https://example.com/a">Read <b>the</b> report</a>"#),
        &[],
    );
    assert_eq!(
        find(&report, "example.com/a").display_text,
        "Read the report"
    );
}

#[test]
fn an_ampersand_next_to_a_multi_byte_character_does_not_abort_the_scan() {
    // `&text[..12]` panics when byte 12 lands inside a character, and ordinary
    // marketing copy is enough to do it. Three call sites are covered here at
    // once: the href, the anchor text, and (in the tables suite) a cell.
    let report = extract_links(
        &html(r#"<a href="https://example.com/a?x=1&amp;y=2">Ben &amp; Jerry's — a treat</a>"#),
        &[],
    );
    let link = find(&report, "example.com/a");
    assert_eq!(link.display_text, "Ben & Jerry's — a treat");
    assert!(
        link.norm.contains("x=1&y=2"),
        "the href decoded too: {}",
        link.norm
    );
}

#[test]
fn a_truncated_entity_whose_window_edge_splits_a_character_is_left_alone() {
    // The exact arithmetic matters, and a probe that is one byte out does not
    // bite: the window is MAX_ENTITY_BYTES from the `&`, so the character has
    // to *straddle* that byte rather than begin at it. `&` at 0, ten ASCII
    // bytes at 1..=10, then a three-byte em dash at 11..=13 — byte 12 is
    // inside it, which is what `&tail[..12]` panics on.
    let hostile = format!("&{}\u{2014}", "a".repeat(MAX_ENTITY_BYTES - 2));
    assert_eq!(hostile.len(), MAX_ENTITY_BYTES + 2);
    assert!(!hostile.is_char_boundary(MAX_ENTITY_BYTES));
    assert_eq!(decode_entities(&hostile), hostile);

    // And the ordinary cases still decode.
    assert_eq!(decode_entities("&#8212;"), "\u{2014}");
    assert_eq!(decode_entities("a &amp; b"), "a & b");
    assert_eq!(decode_entities("&\u{2014}"), "&\u{2014}");
}

#[test]
fn a_repeated_link_collapses_to_one_entry_with_an_occurrence_count() {
    let report = extract_links(
        &html(
            r#"<a href="https://example.com/a">One</a>
               <a href="https://example.com/a">Two</a>
               <a href="https://example.com/a">Three</a>"#,
        ),
        &[],
    );
    assert_eq!(report.links.len(), 1);
    assert_eq!(report.links[0].occurrences, 3);
}

#[test]
fn entity_encoded_query_strings_deduplicate_with_their_decoded_form() {
    let report = extract_links(
        &html(
            r#"<a href="https://example.com/a?x=1&amp;y=2">A</a>
               <a href="https://example.com/a?x=1&y=2">B</a>"#,
        ),
        &[],
    );
    assert_eq!(report.links.len(), 1, "&amp; in an href is an &");
}

#[test]
fn a_default_port_a_fragment_and_a_case_difference_do_not_split_identity() {
    let report = extract_links(
        &html(
            r#"<a href="https://Example.COM/a">A</a>
               <a href="https://example.com:443/a">B</a>
               <a href="https://example.com/a#section">C</a>"#,
        ),
        &[],
    );
    assert_eq!(report.links.len(), 1);
    assert_eq!(report.links[0].occurrences, 3);
}

#[test]
fn a_trailing_slash_is_not_merged_away() {
    let report = extract_links(
        &html(r#"<a href="https://example.com/a">A</a><a href="https://example.com/a/">B</a>"#),
        &[],
    );
    assert_eq!(
        report.links.len(),
        2,
        "plenty of servers serve different resources at these"
    );
}

#[test]
fn a_protocol_relative_target_is_surfaced_and_still_checked() {
    // `//host/path` is a link every browser resolves. Dropping it for want of
    // a `://` meant a spoof written that way vanished from the picker instead
    // of being flagged.
    let report = extract_links(
        &html(r#"<a href="//evil.example.net/login">https://bank.example.com</a>"#),
        &[],
    );
    let link = find(&report, "evil.example.net");
    assert_eq!(link.scheme, "https", "read as the safer of the two");
    assert!(link.deceptive, "and the mismatch is still caught");
}

#[test]
fn a_javascript_or_data_target_is_never_surfaced() {
    let report = extract_links(
        &html(
            r#"<a href="javascript:alert(1)">Click</a>
               <a href="data:text/html,<script>x</script>">Also click</a>
               <a href="file:///etc/passwd">And this</a>"#,
        ),
        &[],
    );
    assert!(
        report.links.is_empty(),
        "only http(s) reaches a picker: {:?}",
        report.links
    );
}

// ---------------------------------------------------------------------------
// The phishing case
// ---------------------------------------------------------------------------

#[test]
fn display_text_naming_another_domain_is_reported_rather_than_hidden() {
    let report = extract_links(
        &html(r#"<a href="https://evil.example.net/login">https://bank.example.com</a>"#),
        &[],
    );
    let link = find(&report, "evil.example.net");
    assert!(
        link.deceptive,
        "the text says one host and the href another"
    );
    assert_eq!(link.display_host.as_deref(), Some("bank.example.com"));
    assert_eq!(
        link.display_text, "https://bank.example.com",
        "the claim is still shown; a picker that quietly replaced it would rob the reader of the fact"
    );
}

#[test]
fn a_bare_domain_as_display_text_is_compared_too() {
    let report = extract_links(
        &html(r#"<a href="https://evil.example.net/login">bank.example.com</a>"#),
        &[],
    );
    assert!(find(&report, "evil.example.net").deceptive);
}

#[test]
fn an_innocuous_first_anchor_does_not_clear_a_later_spoof_of_the_same_target() {
    // The same target twice: harmless text first, a lying one second. A
    // duplicate handler that only ever inspected the first occurrence's text
    // would report this link as honest, which makes "put the safe anchor
    // first" a complete bypass of the flag.
    let report = extract_links(
        &html(
            r#"<a href="https://evil.example.net/login">Click here</a>
               <a href="https://evil.example.net/login">https://bank.example.com</a>"#,
        ),
        &[],
    );
    assert_eq!(report.links.len(), 1, "one target, seen twice");
    let link = find(&report, "evil.example.net");
    assert!(link.deceptive, "the second anchor's claim still counts");
    assert_eq!(link.display_host.as_deref(), Some("bank.example.com"));
    assert_eq!(
        link.display_text, "https://bank.example.com",
        "and the picker shows the claim that lies, not the innocuous one"
    );
}

#[test]
fn a_subdomain_of_the_displayed_domain_is_not_called_deceptive() {
    let report = extract_links(
        &html(r#"<a href="https://secure.bank.example.com/login">bank.example.com</a>"#),
        &[],
    );
    assert!(
        !find(&report, "secure.bank.example.com").deceptive,
        "same registrable domain; crying wolf here trains people to ignore the flag"
    );
}

#[test]
fn a_two_level_public_suffix_is_not_mistaken_for_a_registrable_domain() {
    let report = extract_links(
        &html(r#"<a href="https://evil.co.uk/login">bank.co.uk</a>"#),
        &[],
    );
    assert!(
        find(&report, "evil.co.uk").deceptive,
        "`co.uk` alone is not the registrable domain, so these are different sites"
    );
}

#[test]
fn a_punycode_or_non_ascii_host_is_deceptive_on_its_own() {
    let report = extract_links(
        &html(r#"<a href="https://xn--80ak6aa92e.com/">Apple</a>"#),
        &[],
    );
    assert!(find(&report, "xn--80ak6aa92e").deceptive);

    let report = extract_links(&plain("see https://аpple.com/x for details"), &[]);
    assert!(
        report.links.iter().all(|link| link.deceptive),
        "a Cyrillic lookalike host is not what it appears to be"
    );
}

#[test]
fn a_deceptive_link_is_held_below_an_honest_one_of_the_same_class() {
    let report = extract_links(
        &html(
            r#"<a href="https://zoom.us/j/1">Join the call</a>
               <a href="https://evil.example.net/j/2">https://zoom.us/j/2</a>"#,
        ),
        &[],
    );
    let honest = find(&report, "zoom.us/j/1");
    let spoofed = find(&report, "evil.example.net");
    assert!(
        honest.score > spoofed.score,
        "a one-tap picker must not default to the spoof: {} vs {}",
        honest.score,
        spoofed.score
    );
    assert!(spoofed.deceptive);
}

#[test]
fn display_text_cannot_carry_bidi_overrides_into_a_terminal() {
    let report = extract_links(
        &html("<a href=\"https://example.com/a\">safe\u{202e}moc.live\u{202c}</a>"),
        &[],
    );
    let link = find(&report, "example.com/a");
    assert!(!link.display_text.contains('\u{202e}'));
    assert!(!link.display_text.contains('\u{202c}'));
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

#[test]
fn the_list_unsubscribe_header_outranks_every_heuristic() {
    // A path that looks like nothing in particular, declared by the header.
    let report = extract_links(
        &html(r#"<a href="https://example.com/p/9f2b">Manage preferences</a>"#),
        &["<https://example.com/p/9f2b>, <mailto:u@example.com>".to_owned()],
    );
    let link = find(&report, "example.com/p/9f2b");
    assert_eq!(link.kind, LinkKind::Unsubscribe);
    assert!(link.reason.contains("List-Unsubscribe"));
}

#[test]
fn a_mailto_unsubscribe_is_not_offered_as_something_to_open() {
    let report = extract_links(
        &html(r#"<a href="https://example.com/x">Read</a>"#),
        &["<mailto:unsubscribe@example.com>".to_owned()],
    );
    assert!(
        report
            .links
            .iter()
            .all(|link| link.scheme.starts_with("http")),
        "a pre-addressed message is not a link a picker can open"
    );
}

#[test]
fn an_unsubscribe_path_or_anchor_text_is_recognized_without_a_header() {
    let report = extract_links(
        &html(
            r#"<a href="https://example.com/mail/unsubscribe?id=7">x</a>
               <a href="https://example.com/prefs">Unsubscribe</a>"#,
        ),
        &[],
    );
    assert_eq!(
        find(&report, "unsubscribe?id=7").kind,
        LinkKind::Unsubscribe
    );
    assert_eq!(
        find(&report, "example.com/prefs").kind,
        LinkKind::Unsubscribe
    );
}

#[test]
fn a_click_tracking_redirector_is_classified_and_explicitly_not_resolved() {
    let report = extract_links(
        &html(r#"<a href="https://track.list-manage.com/click?u=1&id=2">Your invoice</a>"#),
        &[],
    );
    let link = find(&report, "list-manage.com");
    assert_eq!(link.kind, LinkKind::Tracking);
    assert!(
        link.reason.contains("not resolved"),
        "the picker says it did not follow the redirect: {:?}",
        link.reason
    );
}

#[test]
fn a_meeting_link_floats_above_a_tracker_and_an_unsubscribe() {
    let report = extract_links(
        &html(
            r#"<a href="https://track.list-manage.com/click?u=1">Open</a>
               <a href="https://example.com/unsubscribe">Unsubscribe</a>
               <a href="https://zoom.us/j/98765">Join</a>"#,
        ),
        &[],
    );
    assert_eq!(
        report.links.first().map(|link| link.kind),
        Some(LinkKind::Meeting),
        "the picker floats the link the reader opened the mail for"
    );
    assert_eq!(
        report.links.last().map(|link| link.kind),
        Some(LinkKind::Tracking)
    );
}

#[test]
fn documents_are_recognized_by_host_and_by_extension() {
    let report = extract_links(
        &html(
            r#"<a href="https://docs.google.com/document/d/abc">Spec</a>
               <a href="https://files.example.com/invoice-2024.pdf">Invoice</a>"#,
        ),
        &[],
    );
    assert_eq!(find(&report, "docs.google.com").kind, LinkKind::Document);
    assert_eq!(find(&report, "invoice-2024.pdf").kind, LinkKind::Document);
}

#[test]
fn a_button_styled_anchor_is_a_call_to_action() {
    let report = extract_links(
        &html(
            r#"<a class="btn btn-primary" href="https://example.com/go">Continue</a>
               <a href="https://example.com/reset">Reset password</a>
               <a href="https://example.com/faq">Frequently asked questions</a>"#,
        ),
        &[],
    );
    assert_eq!(find(&report, "example.com/go").kind, LinkKind::Cta);
    assert_eq!(find(&report, "example.com/reset").kind, LinkKind::Cta);
    assert_eq!(find(&report, "example.com/faq").kind, LinkKind::Other);
}

#[test]
fn a_tracking_pixel_is_counted_and_never_offered_as_a_link() {
    let report = extract_links(
        &html(
            r#"<img src="https://track.example.com/o.gif?id=1" width="1" height="1">
               <img src="https://track.example.com/b.gif" style="display:none">
               <a href="https://example.com/a">Read</a>"#,
        ),
        &[],
    );
    assert_eq!(report.tracking_pixels, 2);
    assert!(
        report.links.iter().all(|link| !link.url.contains("o.gif")),
        "offering a beacon as something to click would fire it"
    );
}

#[test]
fn the_picker_is_ordered_by_score_then_by_first_appearance() {
    let report = extract_links(
        &html(
            r#"<a href="https://example.com/b">Second reference</a>
               <a href="https://example.com/a">First reference</a>"#,
        ),
        &[],
    );
    let scores: Vec<f64> = report.links.iter().map(|link| link.score).collect();
    assert!(
        scores.windows(2).all(|pair| pair[0] >= pair[1]),
        "descending by score: {scores:?}"
    );
    assert_eq!(
        report.links.first().map(|link| link.url.as_str()),
        Some("https://example.com/b"),
        "and a tie goes to whichever came first in the message"
    );
}

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

#[test]
fn an_unclosed_anchor_does_not_send_the_scanner_through_the_document() {
    // The `<a` never closes; a scanner that searched for its `>` would walk
    // the rest of the part once per tag, which is the quadratic case.
    let mut hostile = String::new();
    for _ in 0..2_000 {
        hostile.push_str("<a href=\"https://example.com/a\"");
        hostile.push_str(&"filler ".repeat(20));
    }
    let started = std::time::Instant::now();
    let report = extract_links(&html(&hostile), &[]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the scan stayed linear"
    );
    assert!(
        report.links.is_empty(),
        "an unclosed tag contributes no link"
    );
}

#[test]
fn the_anchor_cap_stops_the_scan_before_the_end_of_a_hostile_part() {
    // Asserting `links.len() <= MAX_LINKS` here would pass with the anchor cap
    // deleted, because the *link* cap bounds that on its own. What the anchor
    // cap does is stop the scan, so the assertion is about a link that exists
    // in the part and is nevertheless never reached: a distinctive host placed
    // after the cap.
    let mut hostile = String::new();
    for index in 0..(MAX_ANCHORS_PER_PART + 500) {
        hostile.push_str(&format!(r#"<a href="https://d{index}.example.com/">x</a>"#));
    }
    hostile.push_str(r#"<a href="https://sentinel.example.org/">last</a>"#);
    let report = extract_links(&html(&hostile), &[]);
    assert!(
        report
            .links
            .iter()
            .all(|link| link.host != "sentinel.example.org"),
        "the scan stopped at the anchor cap rather than walking the whole part"
    );
    assert!(report.links.len() <= MAX_LINKS);
}

#[test]
fn the_link_cap_bounds_the_picker_and_counts_what_it_dropped() {
    let mut body = String::new();
    for index in 0..(MAX_LINKS + 40) {
        body.push_str(&format!("https://example.com/{index} "));
    }
    let report = extract_links(&plain(&body), &[]);
    assert_eq!(report.links.len(), MAX_LINKS);
    assert!(
        report.truncated > 0,
        "the excess is reported rather than silently dropped"
    );
}

#[test]
fn an_oversized_part_is_skipped_rather_than_scanned() {
    let huge = format!(
        "https://example.com/a {}",
        "x".repeat(MAX_PART_BYTES + 1_024)
    );
    let report = extract_links(&plain(&huge), &[]);
    assert_eq!(report.skipped_parts, 1);
    assert!(report.links.is_empty());
}

#[test]
fn the_message_budget_stops_after_enough_parts_however_small_each_one_is() {
    // Each part is well under the per-part cap; only the message-wide budget
    // bounds this, which is the bound a per-part limit alone would miss.
    let part = "y".repeat(MAX_PART_BYTES / 2);
    let parts: Vec<LinkPart> = (0..12)
        .map(|index| LinkPart {
            part: format!("text:{index}"),
            text: format!("https://example.com/{index} {part}"),
            html: false,
        })
        .collect();
    let report = extract_links(&parts, &[]);
    assert!(
        report.skipped_parts > 0,
        "the message-wide budget bound the scan"
    );
    assert!(report.links.len() < parts.len());
}

#[test]
fn an_absurdly_long_url_is_not_retained() {
    let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES * 2));
    let report = extract_links(&plain(&long), &[]);
    assert!(report.links.is_empty(), "a URL no browser would accept");
}

#[test]
fn display_text_is_cut_at_the_cap() {
    let text = "z".repeat(MAX_DISPLAY_CHARS * 4);
    let report = extract_links(
        &html(&format!(r#"<a href="https://example.com/a">{text}</a>"#)),
        &[],
    );
    assert!(find(&report, "example.com/a").display_text.chars().count() <= MAX_DISPLAY_CHARS);
}

// ---------------------------------------------------------------------------
// The model route
// ---------------------------------------------------------------------------

fn two_links() -> LinkReport {
    extract_links(
        &html(
            r#"<a href="https://evil.example.net/login">https://bank.example.com</a>
               <a href="https://example.com/report.pdf">The report</a>"#,
        ),
        &[],
    )
}

#[test]
fn the_model_refines_a_classification_and_is_recorded_as_the_classifier() {
    let report = two_links();
    let index = report
        .links
        .iter()
        .position(|link| link.url.contains("report.pdf"))
        .expect("the report link");
    let answer = serde_json::json!({
        "links": [{"index": index, "kind": "cta", "score": 0.9, "reason": "the message's ask"}],
    });
    let refined = apply_model_answer(report, &answer.to_string()).expect("answer applies");
    let link = find(&refined, "report.pdf");
    assert_eq!(link.kind, LinkKind::Cta);
    assert_eq!(link.classifier, Classifier::Model);
    assert_eq!(link.reason, "the message's ask");
}

#[test]
fn the_model_cannot_add_a_link_or_widen_the_vocabulary() {
    let report = two_links();
    let before = report.links.len();
    let answer = serde_json::json!({
        "links": [
            {"index": 99, "kind": "cta", "score": 1.0, "reason": "not a link in this message"},
            {"index": 0, "kind": "definitely-safe", "score": 1.0, "reason": "invented category"},
        ],
    });
    let refined = apply_model_answer(report, &answer.to_string()).expect("answer applies");
    assert_eq!(
        refined.links.len(),
        before,
        "the picker's contents come from the message"
    );
    assert!(
        refined
            .links
            .iter()
            .all(|link| link.classifier == Classifier::Rules),
        "an unknown kind leaves the deterministic answer standing"
    );
}

#[test]
fn the_model_cannot_talk_a_spoofed_link_above_an_honest_one() {
    let report = two_links();
    let spoof = report
        .links
        .iter()
        .position(|link| link.url.contains("evil.example.net"))
        .expect("the spoofed link");
    let honest = report
        .links
        .iter()
        .position(|link| link.url.contains("report.pdf"))
        .expect("the honest link");
    let answer = serde_json::json!({
        "links": [
            {"index": spoof, "kind": "cta", "score": 1.0, "reason": "totally legitimate"},
            {"index": honest, "kind": "document", "score": 1.0, "reason": "the report"},
        ],
    });
    let refined = apply_model_answer(report, &answer.to_string()).expect("answer applies");
    let spoofed = find(&refined, "evil.example.net");
    let honest = find(&refined, "report.pdf");
    assert!(
        spoofed.deceptive,
        "and the flag is not in the schema to clear"
    );
    assert!(
        honest.score > spoofed.score,
        "the deceptive penalty is re-applied to the model's own score: {} vs {}",
        honest.score,
        spoofed.score
    );
    assert_eq!(
        refined.links.first().map(|link| link.url.as_str()),
        Some("https://example.com/report.pdf"),
        "so the picker still floats the honest link"
    );
}

#[test]
fn the_model_cannot_reclassify_a_link_it_was_never_shown() {
    // `model_listing` truncates at MAX_LINKS_TO_MODEL, so an index past that
    // names a link the model never saw and can have no opinion about.
    let mut body = String::new();
    for index in 0..(MAX_LINKS_TO_MODEL + 20) {
        body.push_str(&format!("https://example.com/{index} "));
    }
    let report = extract_links(&plain(&body), &[]);
    assert!(report.links.len() > MAX_LINKS_TO_MODEL);
    let beyond = MAX_LINKS_TO_MODEL + 5;
    let target = report.links[beyond].url.clone();
    let answer = serde_json::json!({
        "links": [{"index": beyond, "kind": "meeting", "score": 1.0, "reason": "unseen"}],
    });
    let refined = apply_model_answer(report, &answer.to_string()).expect("answer applies");
    let link = find(&refined, &target);
    assert_eq!(link.classifier, Classifier::Rules);
    assert_ne!(link.kind, LinkKind::Meeting);
}

#[test]
fn a_model_answer_that_is_not_the_requested_schema_is_an_internal_error() {
    let error = apply_model_answer(two_links(), "{oh no").expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::Internal);
}

#[test]
fn a_model_reason_cannot_carry_bidi_overrides_into_a_terminal() {
    let report = two_links();
    let answer = serde_json::json!({
        "links": [{"index": 0, "kind": "other", "score": 0.5, "reason": "safe\u{202e}suoregnad\u{202c}"}],
    });
    let refined = apply_model_answer(report, &answer.to_string()).expect("answer applies");
    assert!(refined
        .links
        .iter()
        .all(|link| !link.reason.contains('\u{202e}')));
}

#[test]
fn the_listing_shown_to_the_model_withholds_the_rules_own_verdict() {
    let report = two_links();
    let listing = model_listing(&report.links, 10);
    assert!(listing.contains("target:"));
    assert!(listing.contains("text:"));
    for kind in LinkKind::ALL {
        assert!(
            !listing.contains(&format!("kind: {}", kind.as_str())),
            "a second opinion that was told the first opinion is not one"
        );
    }
    assert!(
        listing.contains("different host"),
        "but the mismatch it cannot see from the strings alone is stated"
    );
}

// ---------------------------------------------------------------------------
// The wire vocabulary
// ---------------------------------------------------------------------------

#[test]
fn every_link_kind_round_trips_through_its_string_form() {
    for kind in LinkKind::ALL {
        assert_eq!(LinkKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(LinkKind::parse("phishing"), None);
}
