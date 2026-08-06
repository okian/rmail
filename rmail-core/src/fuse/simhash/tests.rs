//! What SimHash owes the fusion pipeline: identical text fingerprints
//! identically, a quoted reply/forward stays within the near-dup threshold
//! of its source, and — the direction the task brief calls out as the one
//! that actually matters — two messages that merely share a topic sit well
//! outside it. Collapsing two distinct results is worse than missing an
//! actual duplicate, so the false-positive-avoidance tests here assert a
//! comfortable margin past the threshold, not just "greater than."

use super::*;

/// A long, prose-like shared block — the "original" body both a quoted
/// reply and a forward carry verbatim. Long relative to what gets appended
/// to it below, which is what keeps the appended text's few differing
/// shingles from swamping the shared block's shingles in the per-bit vote
/// (see `simhash.rs`'s module docs on weighting by occurrence count).
const ORIGINAL_BODY: &str = "\
Hi team, quick update on the Q3 roadmap. We are moving the launch date for \
the new billing dashboard to the third week of September so the payments \
squad has time to finish the reconciliation work first. The design review \
happens next Tuesday at 10am, and I would like everyone who touches the \
invoicing flow to attend, including anyone from support who has fielded \
customer questions about the current export format. After the review we \
will finalize the migration plan for existing customers, draft the release \
notes, and schedule a dry run against the staging environment a full week \
before the actual cutover so we have time to fix anything that breaks \
without pressure. Please reply here with any blockers you are already \
aware of so we can bring them up on Tuesday instead of discovering them \
during the dry run itself. On the engineering side, the backend team has \
already merged the new ledger reconciliation service behind a feature \
flag, and the frontend team is finishing the redesigned invoice table with \
the new filtering and export controls customers have been asking for \
since the spring survey. QA has a full regression pass scheduled for the \
following Monday, covering both the legacy CSV export and the new PDF \
export path, and we will need at least two people from support shadowing \
that pass so they can flag anything that looks confusing from a \
customer's perspective rather than just a technical one. Finance has \
already signed off on the new rounding rules for partial refunds, so that \
piece should not block the review. If anyone is out next week, please \
name a delegate before Friday so the review does not need to be \
rescheduled a second time. I will send the finalized agenda and a short \
pre-read on Monday morning covering the open questions from last week's \
sync.";

#[test]
fn identical_text_fingerprints_identically() {
    let a = fingerprint(ORIGINAL_BODY).expect("long body has a fingerprint");
    let b = fingerprint(ORIGINAL_BODY).expect("long body has a fingerprint");
    assert_eq!(a, b);
    assert_eq!(hamming_distance(a, b), 0);
    assert!(is_near_duplicate(a, b));
}

#[test]
fn a_short_quoted_reply_is_a_near_duplicate_of_the_original() {
    let reply = format!("Thanks!\n\n> {ORIGINAL_BODY}");
    let original_fp = fingerprint(ORIGINAL_BODY).expect("original has a fingerprint");
    let reply_fp = fingerprint(&reply).expect("reply has a fingerprint");
    let distance = hamming_distance(original_fp, reply_fp);
    assert!(
        distance <= NEAR_DUP_HAMMING_THRESHOLD,
        "a short reply quoting the whole original verbatim should collapse \
         (distance {distance}, threshold {NEAR_DUP_HAMMING_THRESHOLD})"
    );
}

#[test]
fn a_forwarded_copy_is_a_near_duplicate_of_the_original() {
    let forwarded = format!("Fwd:\n\n{ORIGINAL_BODY}");
    let original_fp = fingerprint(ORIGINAL_BODY).expect("original has a fingerprint");
    let forwarded_fp = fingerprint(&forwarded).expect("forwarded copy has a fingerprint");
    let distance = hamming_distance(original_fp, forwarded_fp);
    assert!(
        distance <= NEAR_DUP_HAMMING_THRESHOLD,
        "a forward wrapping the same body verbatim should collapse \
         (distance {distance}, threshold {NEAR_DUP_HAMMING_THRESHOLD})"
    );
}

#[test]
fn a_newsletter_resend_with_a_different_tracking_id_is_a_near_duplicate() {
    // The realistic bulk-newsletter case: the body is identical except for
    // a per-recipient unsubscribe/tracking token.
    let first = format!("{ORIGINAL_BODY}\n\nUnsubscribe: https://list.example.com/u/aa11bb22cc33");
    let second = format!("{ORIGINAL_BODY}\n\nUnsubscribe: https://list.example.com/u/zz99yy88xx77");
    let fp1 = fingerprint(&first).expect("resend has a fingerprint");
    let fp2 = fingerprint(&second).expect("resend has a fingerprint");
    let distance = hamming_distance(fp1, fp2);
    assert!(
        distance <= NEAR_DUP_HAMMING_THRESHOLD,
        "two resends differing only by tracking id should collapse \
         (distance {distance}, threshold {NEAR_DUP_HAMMING_THRESHOLD})"
    );
}

#[test]
fn merely_similar_messages_stay_well_outside_the_threshold() {
    // Same rough topic (a roadmap/scheduling update) and comparable length
    // to `ORIGINAL_BODY`, but a materially different message: different
    // dates, different deliverable, different ask. This is the
    // false-positive direction the task brief calls out as the one that
    // matters most -- collapsing these would hide one of two real results.
    let different = "\
Hey everyone, wanted to flag a change to the mobile release schedule. \
We are pushing the app store submission for the offline-sync feature out \
to the second week of October because the crash reports from the beta \
channel need another pass before we can ship confidently to everyone. \
There will be a triage meeting Thursday afternoon and I want the mobile \
platform folks plus anyone on QA who ran the beta build to be there so we \
can walk through the top crash signatures together. Once triage wraps we \
will decide whether to cut a new beta or go straight to a release \
candidate, write up the known-issues doc for support, and lock the App \
Store listing copy. Let me know now if you already have concerns about \
the new date so we are not surprised by them Thursday. On the engineering \
side, the platform team already isolated two of the three top crash \
signatures to a race condition in the local cache layer, and a fix is in \
review now; the third signature is still unclear and might need a repro \
device from one of the affected beta testers before we can make any real \
progress on it. Design is finishing a small in-app banner explaining the \
sync delay to beta users so we are not silently missing their feedback \
while the fix lands. Marketing has asked whether the public launch date \
needs to move as well, and my current read is no, as long as the release \
candidate build passes triage by the following Monday, but I want \
engineering's honest read on that before I tell them anything definite. \
If the fix does not land in time we will need a fallback plan for the \
store listing, so please raise it now rather than the week we are \
actually supposed to submit.";

    let original_fp = fingerprint(ORIGINAL_BODY).expect("original has a fingerprint");
    let different_fp = fingerprint(different).expect("different message has a fingerprint");
    let distance = hamming_distance(original_fp, different_fp);
    assert!(
        distance > NEAR_DUP_HAMMING_THRESHOLD,
        "two distinct messages on a similar topic must not collapse \
         (distance {distance}, threshold {NEAR_DUP_HAMMING_THRESHOLD})"
    );
    // A comfortable margin, not just "technically over the line" -- the
    // false-positive direction matters more than the false-negative one
    // (task brief), so this asserts real separation rather than a distance
    // of, say, 4 against a threshold of 3.
    assert!(
        distance >= NEAR_DUP_HAMMING_THRESHOLD + 3,
        "expected a comfortable margin past the threshold, got distance {distance}"
    );
    assert!(!is_near_duplicate(original_fp, different_fp));
}

#[test]
fn short_text_has_no_fingerprint() {
    assert_eq!(fingerprint(""), None);
    assert_eq!(
        fingerprint("hello"),
        None,
        "one word has no bigram to shingle"
    );
    assert_eq!(fingerprint("hello world"), None, "well under the minimum");
}

#[test]
fn short_stock_replies_never_fingerprint_and_so_never_falsely_collapse() {
    // The exact false-positive class the 12-token minimum exists to kill:
    // two *unrelated* messages sharing a common short reply phrase must not
    // fingerprint identically just because there was nothing else to go on.
    // Before this bar existed, each of these produced a real fingerprint
    // from one or two shingles, so any two unrelated threads using the same
    // stock phrase collapsed at distance 0 -- see mod.rs's `simhash.rs`
    // "Why a minimum of 12 tokens" doc section for the worked example (an
    // "lgtm" search returning one result instead of forty).
    for phrase in ["ok thanks", "lgtm ship it", "sounds good to me"] {
        assert_eq!(
            fingerprint(phrase),
            None,
            "{phrase:?} is short enough that it must never fingerprint"
        );
    }
}

#[test]
fn the_minimum_token_boundary_is_exact() {
    // 11 tokens: still below the bar. 12: right at it.
    let eleven = "one two three four five six seven eight nine ten eleven";
    let twelve = "one two three four five six seven eight nine ten eleven twelve";
    assert_eq!(fingerprint(eleven), None);
    assert!(fingerprint(twelve).is_some());
}

#[test]
fn hamming_distance_counts_differing_bits() {
    // Hand-computed: 0b1010 ^ 0b1100 = 0b0110, which has exactly two set
    // bits, independent of anything the fingerprint function does.
    assert_eq!(hamming_distance(0b1010, 0b1100), 2);
    assert_eq!(hamming_distance(0b1111, 0b1111), 0);
    assert_eq!(hamming_distance(0, u64::MAX), 64);
}

#[test]
fn near_duplicate_threshold_is_inclusive_at_the_boundary() {
    // Two fingerprints differing in exactly the threshold's worth of bits
    // (the low 3 bits) must still count as near-duplicate; one more bit
    // must not.
    let base: u64 = 0b1010_1010_1010_1010;
    let at_threshold = base ^ 0b0000_0111; // 3 bits flipped
    let past_threshold = base ^ 0b0000_1111; // 4 bits flipped
    assert_eq!(hamming_distance(base, at_threshold), 3);
    assert!(is_near_duplicate(base, at_threshold));
    assert_eq!(hamming_distance(base, past_threshold), 4);
    assert!(!is_near_duplicate(base, past_threshold));
}

#[test]
fn word_order_changes_the_fingerprint() {
    // Bigram shingling should distinguish a reordering from the original --
    // a pure bag-of-words (unigram) scheme would fingerprint these
    // identically, which is the looseness `simhash.rs`'s module docs argue
    // against.
    let a = fingerprint("the quick brown fox jumps swiftly over the lazy dog today again")
        .expect("has a fingerprint");
    let b = fingerprint("again today dog lazy the over swiftly jumps fox brown quick the")
        .expect("has a fingerprint");
    assert_ne!(a, b, "reversing word order should change the fingerprint");
}
