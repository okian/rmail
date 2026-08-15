use super::{Entry, FinderStore, Limits};
use crate::config::FinderConfig;
use crate::finder::rank::Signals;
use crate::finder::ItemKind;

fn entry(kind: ItemKind, ref_id: i64, text: &str) -> Entry {
    Entry::new(ref_id, kind, ref_id, 1, 0, text, "", &Signals::default())
}

fn unlimited() -> Limits {
    Limits {
        max_entries: usize::MAX,
        max_bytes: usize::MAX,
    }
}

// ---------------------------------------------------------------------------
// the single-buffer layout
// ---------------------------------------------------------------------------

#[test]
fn the_accessors_split_the_shared_buffer_correctly() {
    let entry = Entry::new(
        1,
        ItemKind::Message,
        1,
        1,
        2,
        "Invoice 338",
        "billing@acme.com",
        &Signals::default(),
    );
    assert_eq!(entry.primary_text(), "Invoice 338");
    assert_eq!(entry.secondary(), "billing@acme.com");
    assert_eq!(entry.blob(), "Invoice 338 billing@acme.com");
}

#[test]
fn an_entry_with_no_secondary_has_an_empty_one() {
    let entry = entry(ItemKind::Tag, 1, "project/alpha");
    assert_eq!(entry.primary_text(), "project/alpha");
    assert_eq!(entry.secondary(), "");
    assert_eq!(entry.blob(), "project/alpha");
}

/// The whole point of the layout: ASCII text is stored once, not twice.
#[test]
fn ascii_text_does_not_pay_for_a_second_folded_copy() {
    let ascii = Entry::new(
        1,
        ItemKind::Message,
        1,
        1,
        2,
        "Re: Q3 planning follow-up",
        "Dana Whitfield <dana@example.com>",
        &Signals::default(),
    );
    let text_len =
        "Re: Q3 planning follow-up".len() + 1 + "Dana Whitfield <dana@example.com>".len();
    assert_eq!(
        ascii.footprint(),
        std::mem::size_of::<Entry>() + text_len,
        "an ASCII entry must hold exactly one copy of its text"
    );

    // ...and text that folding actually changes still gets its own blob.
    let folded = Entry::new(
        1,
        ItemKind::Message,
        1,
        1,
        2,
        "Café",
        "",
        &Signals::default(),
    );
    assert_eq!(folded.primary_text(), "Café");
    assert_eq!(folded.blob(), "Cafe");
    assert!(folded.footprint() > std::mem::size_of::<Entry>() + "Café".len());
}

/// prd.md's memory model is "~100 bytes + blob per entry". The per-entry
/// *header* is the half that no amount of short subject lines can shrink, so
/// it is pinned directly here rather than only through the aggregate budget:
/// 100k entries leave the 25 MB cap ~30% of headroom, which is enough to
/// absorb a layout regression without the budget test noticing. This one
/// notices — every 8 bytes added to the struct is 800 KB across a full index.
///
/// A failure here is not automatically a bug; it is a decision to make on
/// purpose. Widen the ceiling only alongside the budget it feeds.
#[test]
fn the_entry_header_matches_the_prd_memory_model() {
    let header = std::mem::size_of::<Entry>();
    assert!(
        header <= 112,
        "an entry's header grew to {header} bytes, past prd.md's ~100-byte model"
    );
}

/// Multi-byte text must not be sliced through a character — the split point
/// is a byte length, and the accessor has to keep it a boundary.
#[test]
fn a_multibyte_primary_splits_on_a_char_boundary() {
    let entry = Entry::new(
        1,
        ItemKind::Contact,
        1,
        0,
        0,
        "Renée Côté",
        "renee@example.com",
        &Signals::default(),
    );
    assert_eq!(entry.primary_text(), "Renée Côté");
    assert_eq!(entry.secondary(), "renee@example.com");
    assert_eq!(entry.blob(), "Renee Cote renee@example.com");
    // The boundary the position mapper trusts.
    assert_eq!(
        entry.primary_folded_len as usize,
        "Renee Cote".chars().count()
    );
}

/// The blob's `MAX_MATCH_CHARS` cap is what bounds the aligner, and it has to
/// be applied where the entry is built, not hoped for upstream.
#[test]
fn an_absurd_subject_is_capped_at_the_dp_bound() {
    let huge = "a".repeat(crate::finder::score::MAX_MATCH_CHARS * 10);
    let entry = entry(ItemKind::Message, 1, &huge);
    assert_eq!(
        entry.blob().chars().count(),
        crate::finder::score::MAX_MATCH_CHARS
    );
    // The display text is untouched — the cap bounds matching, not rendering.
    assert_eq!(entry.primary_text().len(), huge.len());
}

#[test]
fn signals_round_trip_through_the_packed_fields() {
    let signals = Signals {
        last_activity: Some(1_800_000_000),
        unread: true,
        importance: 1.0,
        frequency: 42,
    };
    let entry = Entry::new(1, ItemKind::Message, 1, 1, 2, "s", "", &signals);
    assert_eq!(entry.signals(), signals);

    // A negative frequency (impossible from SQL, cheap to be right about)
    // must not wrap to four billion.
    let entry = Entry::new(
        1,
        ItemKind::Message,
        1,
        1,
        2,
        "s",
        "",
        &Signals {
            frequency: -7,
            ..Signals::default()
        },
    );
    assert_eq!(entry.signals().frequency, 0);
}

// ---------------------------------------------------------------------------
// the store
// ---------------------------------------------------------------------------

#[test]
fn an_upsert_replaces_rather_than_duplicates() {
    let mut store = FinderStore::new();
    let limits = unlimited();
    assert!(store.upsert(entry(ItemKind::Message, 1, "first"), &limits));
    assert!(store.upsert(entry(ItemKind::Message, 1, "second"), &limits));
    assert_eq!(store.len(), 1);
    assert_eq!(store.entries(ItemKind::Message)[0].primary_text(), "second");
}

#[test]
fn kinds_are_stored_separately() {
    let mut store = FinderStore::new();
    let limits = unlimited();
    // Same ref_id in two kinds is a normal state of affairs: message 1 and
    // mailbox 1 are unrelated rows.
    store.upsert(entry(ItemKind::Message, 1, "a message"), &limits);
    store.upsert(entry(ItemKind::Mailbox, 1, "a mailbox"), &limits);
    assert_eq!(store.len(), 2);
    assert_eq!(store.entries(ItemKind::Message).len(), 1);
    assert_eq!(store.entries(ItemKind::Mailbox).len(), 1);
}

/// `swap_remove` moves the last element into the freed slot, and the slot map
/// has to follow it — otherwise the *moved* entry becomes unreachable and a
/// later upsert duplicates it.
#[test]
fn removing_from_the_middle_keeps_the_slot_map_correct() {
    let mut store = FinderStore::new();
    let limits = unlimited();
    for id in 1..=3 {
        store.upsert(entry(ItemKind::Message, id, &format!("m{id}")), &limits);
    }
    store.remove(ItemKind::Message, 1);
    assert_eq!(store.len(), 2);

    // Entry 3 was swapped into slot 0. Upserting it must replace, not append.
    store.upsert(entry(ItemKind::Message, 3, "m3 updated"), &limits);
    assert_eq!(store.len(), 2);
    let texts: Vec<&str> = store
        .entries(ItemKind::Message)
        .iter()
        .map(Entry::primary_text)
        .collect();
    assert!(texts.contains(&"m3 updated"), "got {texts:?}");
    assert!(texts.contains(&"m2"), "got {texts:?}");
}

/// The change feed is allowed to be redundant (a drain re-applies its own
/// deletes on the following pass), so a missing key must be a no-op.
#[test]
fn removing_something_absent_is_harmless() {
    let mut store = FinderStore::new();
    store.remove(ItemKind::Message, 999);
    assert_eq!(store.len(), 0);
    assert_eq!(store.footprint(), 0);
}

#[test]
fn the_footprint_grows_and_shrinks_with_the_entries() {
    let mut store = FinderStore::new();
    let limits = unlimited();
    assert_eq!(store.footprint(), 0);
    store.upsert(
        entry(ItemKind::Message, 1, "a fairly long subject line"),
        &limits,
    );
    let with_one = store.footprint();
    assert!(with_one > 0);
    store.upsert(entry(ItemKind::Message, 2, "another one"), &limits);
    assert!(store.footprint() > with_one);
    store.remove(ItemKind::Message, 2);
    assert_eq!(store.footprint(), with_one);
    store.clear();
    assert_eq!(store.footprint(), 0);
    assert!(store.is_empty());
}

/// Replacing an entry with a *smaller* one has to give the bytes back, or the
/// running total drifts upward until the cap refuses everything.
#[test]
fn replacing_with_a_shorter_entry_reclaims_bytes() {
    let mut store = FinderStore::new();
    let limits = unlimited();
    store.upsert(
        entry(ItemKind::Message, 1, "a very long subject line indeed"),
        &limits,
    );
    let big = store.footprint();
    store.upsert(entry(ItemKind::Message, 1, "short"), &limits);
    assert!(store.footprint() < big, "{} vs {big}", store.footprint());
    assert_eq!(store.len(), 1);
}

/// The first of the two explicit bounds. Without it, "load the index into
/// memory" is an unbounded allocation and an unbounded scan.
#[test]
fn the_entry_cap_stops_admissions() {
    let mut store = FinderStore::new();
    let limits = Limits {
        max_entries: 10,
        max_bytes: usize::MAX,
    };
    for id in 1..=50 {
        store.upsert(
            entry(ItemKind::Message, id, &format!("subject {id}")),
            &limits,
        );
    }
    assert_eq!(store.len(), 10);
    assert_eq!(store.rejected(), 40);
}

/// prd.md's "< 25 MB for 100k messages", enforced against measured capacity
/// rather than an estimate per row.
#[test]
fn the_byte_cap_stops_admissions() {
    let mut store = FinderStore::new();
    let one = entry(ItemKind::Message, 1, "a subject line of some length");
    let limits = Limits {
        max_entries: usize::MAX,
        // Room for about three entries.
        max_bytes: one.footprint() * 3 + 8,
    };
    for id in 1..=100 {
        store.upsert(
            entry(ItemKind::Message, id, "a subject line of some length"),
            &limits,
        );
    }
    assert!(store.len() <= 3, "held {} entries", store.len());
    assert!(
        store.footprint() <= limits.max_bytes,
        "{} bytes exceeds the {} cap",
        store.footprint(),
        limits.max_bytes
    );
    assert!(store.rejected() > 0);
}

/// A cap must never make the index go *stale*: refusing an update would leave
/// the old copy resident, which is worse than being one entry over budget.
#[test]
fn a_replacement_is_admitted_even_at_the_cap() {
    let mut store = FinderStore::new();
    let limits = Limits {
        max_entries: 1,
        max_bytes: usize::MAX,
    };
    assert!(store.upsert(entry(ItemKind::Message, 1, "old"), &limits));
    assert!(!store.upsert(entry(ItemKind::Message, 2, "new row"), &limits));
    assert!(store.upsert(entry(ItemKind::Message, 1, "updated"), &limits));
    assert_eq!(
        store.entries(ItemKind::Message)[0].primary_text(),
        "updated"
    );
}

/// The default config has to actually produce prd.md's stated budget, or the
/// bound is documented rather than enforced.
#[test]
fn the_default_limits_match_the_prd_budget() {
    let limits = Limits::from_config(&FinderConfig::default());
    assert_eq!(limits.max_bytes, 25 * 1024 * 1024);
    assert_eq!(limits.max_entries, 200_000);
}

/// A zero cap admits nothing, so the finder would answer every query with an
/// empty list while `IndexStatus` reported a perfectly healthy empty index.
/// Falling back to the default keeps a misconfiguration visible instead of
/// silent.
#[test]
fn a_zero_cap_falls_back_to_the_default() {
    let defaults = Limits::from_config(&FinderConfig::default());

    let limits = Limits::from_config(&FinderConfig {
        max_entries: 0,
        ..FinderConfig::default()
    });
    assert_eq!(limits.max_entries, defaults.max_entries);

    let limits = Limits::from_config(&FinderConfig {
        max_memory_mb: 0,
        ..FinderConfig::default()
    });
    assert_eq!(limits.max_bytes, defaults.max_bytes);

    // ...and a store built with either actually admits something.
    let mut store = FinderStore::new();
    assert!(store.upsert(entry(ItemKind::Message, 1, "a subject"), &limits));
}

/// prd.md's headline claim, measured rather than assumed: a hundred thousand
/// realistic message entries must fit inside the default 25 MB budget. This
/// is what the single-buffer layout in `Entry` exists for — the obvious
/// three-`String` version overshoots by roughly half again.
#[test]
fn a_hundred_thousand_messages_fit_the_default_budget() {
    let limits = Limits::from_config(&FinderConfig::default());
    let mut store = FinderStore::new();
    for id in 0..100_000i64 {
        let admitted = store.upsert(
            Entry::new(
                id,
                ItemKind::Message,
                id,
                1,
                2,
                "Re: Q3 planning follow-up and next steps",
                "Dana Whitfield <dana@example.com>",
                &Signals {
                    last_activity: Some(1_800_000_000 - id),
                    unread: id % 7 == 0,
                    importance: 0.0,
                    frequency: 0,
                },
            ),
            &limits,
        );
        if !admitted {
            break;
        }
    }
    assert_eq!(
        store.len(),
        100_000,
        "the default budget must hold 100k messages; it held {} using {} bytes",
        store.len(),
        store.footprint()
    );
    assert!(store.footprint() <= limits.max_bytes);
}
