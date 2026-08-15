use std::ops::ControlFlow;
use std::sync::{Arc, RwLock};

use tokio_util::sync::CancellationToken;

use super::rank::Signals;
use super::store::{Entry, FinderStore, Limits};
use super::{
    scan, Batch, FindQuery, Finder, ItemKind, Query, ScanStats, Scope, BATCH_STRIDE,
    MAX_INTERMEDIATE_BATCHES, MAX_QUERY_INPUT_CHARS,
};
use crate::config::{FinderConfig, FinderRanking, FinderScope};

const NOW: i64 = 1_800_000_000;

fn unlimited() -> Limits {
    Limits {
        max_entries: usize::MAX,
        max_bytes: usize::MAX,
    }
}

/// A store built from `(kind, ref_id, primary, secondary)` rows.
fn store_of(rows: &[(ItemKind, i64, &str, &str)]) -> FinderStore {
    let mut store = FinderStore::new();
    let limits = unlimited();
    for (kind, ref_id, primary, secondary) in rows {
        store.upsert(
            Entry::new(
                *ref_id,
                *kind,
                *ref_id,
                1,
                if *kind == ItemKind::Message { 2 } else { 0 },
                primary,
                secondary,
                &Signals {
                    last_activity: Some(NOW - ref_id),
                    ..Signals::default()
                },
            ),
            &limits,
        );
    }
    store
}

fn query(text: &str) -> FindQuery {
    FindQuery {
        text: text.to_owned(),
        scope: Scope::All,
        account_id: None,
        mailbox_id: None,
        limit: 20,
        with_positions: true,
    }
}

/// Run one scan and return every batch it emitted.
fn run(store: &FinderStore, query: &FindQuery) -> Vec<Batch> {
    run_with(
        store,
        query,
        &CancellationToken::new(),
        &FinderRanking::default(),
    )
}

fn run_with(
    store: &FinderStore,
    query: &FindQuery,
    cancel: &CancellationToken,
    ranking: &FinderRanking,
) -> Vec<Batch> {
    let mut batches = Vec::new();
    let mut sink = |batch: Batch| {
        batches.push(batch);
        ControlFlow::Continue(())
    };
    scan(store, query, ranking, NOW, cancel, &mut sink);
    batches
}

/// The final, authoritative batch of a scan.
fn final_batch(store: &FinderStore, query: &FindQuery) -> Batch {
    let batches = run(store, query);
    batches
        .last()
        .cloned()
        .expect("a scan always ends with a batch")
}

fn texts(batch: &Batch) -> Vec<&str> {
    batch
        .items
        .iter()
        .map(|item| item.primary_text.as_str())
        .collect()
}

// ---------------------------------------------------------------------------
// sigils and scopes
// ---------------------------------------------------------------------------

/// prd.md's sigil table, all five of them, stripped before matching.
#[test]
fn every_sigil_selects_its_scope_and_is_stripped() {
    for (input, scope, text) in [
        (">arch", Scope::Only(ItemKind::Command), "arch"),
        ("#work", Scope::Only(ItemKind::Tag), "work"),
        ("@ali", Scope::Only(ItemKind::Contact), "ali"),
        ("/weekly", Scope::Only(ItemKind::SavedSearch), "weekly"),
        (":Inbox", Scope::Only(ItemKind::Mailbox), "Inbox"),
    ] {
        let parsed = Query::parse(input, Scope::All);
        assert_eq!(parsed.scope, scope, "for {input:?}");
        assert_eq!(parsed.text, text, "for {input:?}");
        assert!(parsed.sigil, "for {input:?}");
    }
}

#[test]
fn no_sigil_inherits_the_default_scope() {
    let parsed = Query::parse("acme invoice", Scope::Only(ItemKind::Message));
    assert_eq!(parsed.scope, Scope::Only(ItemKind::Message));
    assert_eq!(parsed.text, "acme invoice");
    assert!(!parsed.sigil);
}

/// A lone sigil is a scope switch with an empty query, which is how a palette
/// gets opened showing every command.
#[test]
fn a_lone_sigil_selects_a_scope_and_leaves_no_text() {
    let parsed = Query::parse(">", Scope::All);
    assert_eq!(parsed.scope, Scope::Only(ItemKind::Command));
    assert_eq!(parsed.text, "");
}

/// A sigil inside the query is text, not syntax — `mail find "a@b"` is
/// looking for an address, not switching to contacts.
#[test]
fn a_sigil_only_counts_at_the_start() {
    let parsed = Query::parse("a@b.com", Scope::All);
    assert_eq!(parsed.scope, Scope::All);
    assert_eq!(parsed.text, "a@b.com");
}

#[test]
fn a_mailbox_path_after_a_sigil_keeps_its_slashes() {
    let parsed = Query::parse(":Work/Clients", Scope::All);
    assert_eq!(parsed.scope, Scope::Only(ItemKind::Mailbox));
    assert_eq!(parsed.text, "Work/Clients");
}

/// `FindRequest.query` is a proto `string`, so a client can send megabytes of
/// it. `MAX_QUERY_CHARS` bounds the matcher but is applied *after* folding,
/// so without this the fold itself is unbounded work on every keystroke.
#[test]
fn an_absurd_prompt_is_clamped_before_it_is_folded() {
    let huge = "a".repeat(MAX_QUERY_INPUT_CHARS * 4);
    let parsed = Query::parse(&huge, Scope::All);
    assert_eq!(parsed.text.chars().count(), MAX_QUERY_INPUT_CHARS);

    // The clamp applies past a sigil too, which is the path a palette uses.
    let parsed = Query::parse(&format!(">{huge}"), Scope::All);
    assert_eq!(parsed.scope, Scope::Only(ItemKind::Command));
    assert_eq!(parsed.text.chars().count(), MAX_QUERY_INPUT_CHARS);

    // An ordinary prompt is untouched.
    let parsed = Query::parse("acme invoice", Scope::All);
    assert_eq!(parsed.text, "acme invoice");
}

/// The clamp counts characters, not bytes — a byte cut would produce invalid
/// UTF-8, and a `String` cannot hold that.
#[test]
fn the_prompt_clamp_counts_characters() {
    let huge = "é".repeat(MAX_QUERY_INPUT_CHARS * 2);
    let parsed = Query::parse(&huge, Scope::All);
    assert_eq!(parsed.text.chars().count(), MAX_QUERY_INPUT_CHARS);
    assert!(
        parsed.text.len() > MAX_QUERY_INPUT_CHARS,
        "these are 2-byte chars"
    );
}

#[test]
fn scope_names_round_trip() {
    for scope in [
        Scope::All,
        Scope::Only(ItemKind::Message),
        Scope::Only(ItemKind::Mailbox),
        Scope::Only(ItemKind::Contact),
        Scope::Only(ItemKind::SavedSearch),
        Scope::Only(ItemKind::Tag),
        Scope::Only(ItemKind::Command),
    ] {
        assert_eq!(Scope::from_id(scope.id()), Some(scope), "{}", scope.id());
    }
    assert_eq!(Scope::from_id("nonsense"), None);
    // The config enum's own spelling of "folders" has to resolve too.
    assert_eq!(
        Scope::from_id("folders"),
        Some(Scope::Only(ItemKind::Mailbox))
    );
}

#[test]
fn every_config_scope_maps_to_a_scope() {
    for (config, scope) in [
        (FinderScope::All, Scope::All),
        (FinderScope::Messages, Scope::Only(ItemKind::Message)),
        (FinderScope::Contacts, Scope::Only(ItemKind::Contact)),
        (FinderScope::Folders, Scope::Only(ItemKind::Mailbox)),
        (FinderScope::Tags, Scope::Only(ItemKind::Tag)),
        (
            FinderScope::SavedSearches,
            Scope::Only(ItemKind::SavedSearch),
        ),
        (FinderScope::Commands, Scope::Only(ItemKind::Command)),
    ] {
        assert_eq!(Scope::from(config), scope);
    }
}

/// The storage codes are persisted in `finder_index.kind`; renaming a variant
/// must not renumber them.
#[test]
fn kind_codes_round_trip_and_are_stable() {
    for kind in ItemKind::ALL {
        assert_eq!(ItemKind::from_code(kind.code()), Some(kind));
        assert_eq!(kind.slot(), usize::try_from(kind.code()).expect("small"));
    }
    assert_eq!(ItemKind::Message.code(), 0);
    assert_eq!(ItemKind::Command.code(), 5);
    // A code from a newer schema is skipped, not guessed at.
    assert_eq!(ItemKind::from_code(99), None);
}

// ---------------------------------------------------------------------------
// scanning
// ---------------------------------------------------------------------------

#[test]
fn a_scan_returns_the_matching_entries_best_first() {
    let store = store_of(&[
        (ItemKind::Message, 1, "Acme invoice 338", "billing@acme.com"),
        (ItemKind::Message, 2, "Lunch plans", "sam@example.com"),
        (
            ItemKind::Message,
            3,
            "Acme contract renewal",
            "legal@acme.com",
        ),
    ]);
    let batch = final_batch(&store, &query("acme"));
    assert!(batch.complete);
    let found = texts(&batch);
    assert_eq!(found.len(), 2, "got {found:?}");
    assert!(found.iter().all(|t| t.starts_with("Acme")), "got {found:?}");
}

#[test]
fn a_scope_restricts_which_kinds_are_walked() {
    let store = store_of(&[
        (ItemKind::Message, 1, "work notes", ""),
        (ItemKind::Tag, 2, "work", ""),
        (ItemKind::Mailbox, 3, "Work", ""),
    ]);
    let mut q = query("work");
    q.scope = Scope::Only(ItemKind::Tag);
    let batch = final_batch(&store, &q);
    assert_eq!(texts(&batch), vec!["work"]);
    assert_eq!(batch.items[0].kind, ItemKind::Tag);
}

/// prd.md's `in-folder` scope. Only messages carry a mailbox, so the filter
/// also has to skip every other kind rather than matching them anyway.
#[test]
fn a_mailbox_filter_restricts_to_that_folder_and_to_messages() {
    let mut store = store_of(&[(ItemKind::Message, 1, "release notes", "")]);
    let limits = unlimited();
    store.upsert(
        Entry::new(
            2,
            ItemKind::Message,
            2,
            1,
            99,
            "release notes elsewhere",
            "",
            &Signals::default(),
        ),
        &limits,
    );
    store.upsert(
        Entry::new(
            3,
            ItemKind::Tag,
            3,
            1,
            0,
            "release",
            "",
            &Signals::default(),
        ),
        &limits,
    );

    let mut q = query("release");
    q.mailbox_id = Some(2);
    let batch = final_batch(&store, &q);
    assert_eq!(texts(&batch), vec!["release notes"]);
}

/// A kind with no account of its own (a command, a contact) must survive an
/// account filter, or `mail find --account 1 ">arch"` would return nothing.
#[test]
fn an_account_filter_keeps_account_less_kinds() {
    let mut store = FinderStore::new();
    let limits = unlimited();
    store.upsert(
        Entry::new(
            1,
            ItemKind::Message,
            1,
            1,
            2,
            "archive this",
            "",
            &Signals::default(),
        ),
        &limits,
    );
    store.upsert(
        Entry::new(
            2,
            ItemKind::Message,
            2,
            7,
            2,
            "archive that",
            "",
            &Signals::default(),
        ),
        &limits,
    );
    store.upsert(
        Entry::new(
            3,
            ItemKind::Command,
            3,
            0,
            0,
            "archive",
            "message.archive",
            &Signals::default(),
        ),
        &limits,
    );

    let mut q = query("archive");
    q.account_id = Some(1);
    let batch = final_batch(&store, &q);
    let found = texts(&batch);
    assert!(found.contains(&"archive this"), "got {found:?}");
    assert!(found.contains(&"archive"), "got {found:?}");
    assert!(!found.contains(&"archive that"), "got {found:?}");
}

/// prd.md: "Empty query -> signal-ranked recents/frequent/all-commands."
#[test]
fn an_empty_query_returns_everything_ranked_by_signals() {
    let store = store_of(&[
        (ItemKind::Message, 1, "one", ""),
        (ItemKind::Message, 2, "two", ""),
        (ItemKind::Command, 3, "archive", "message.archive"),
    ]);
    let batch = final_batch(&store, &query(""));
    assert_eq!(batch.items.len(), 3);
    assert!(batch.items.iter().all(|item| item.positions.is_empty()));
}

#[test]
fn a_query_that_matches_nothing_still_completes() {
    let store = store_of(&[(ItemKind::Message, 1, "one", "")]);
    let batch = final_batch(&store, &query("zzzzz"));
    assert!(batch.complete);
    assert!(batch.items.is_empty());
}

/// The bounded top-K: the heap holds `limit` and no more, and what it holds
/// is the *best* `limit`.
#[test]
fn the_result_set_is_bounded_by_the_limit_and_keeps_the_best() {
    let rows: Vec<(ItemKind, i64, String, String)> = (1..=50)
        .map(|id| {
            (
                ItemKind::Message,
                id,
                format!("acme item {id}"),
                String::new(),
            )
        })
        .collect();
    let borrowed: Vec<(ItemKind, i64, &str, &str)> = rows
        .iter()
        .map(|(k, id, p, s)| (*k, *id, p.as_str(), s.as_str()))
        .collect();
    let store = store_of(&borrowed);

    let mut q = query("acme");
    q.limit = 5;
    let batch = final_batch(&store, &q);
    assert_eq!(batch.items.len(), 5);
    // Descending by score, always.
    assert!(
        batch.items.windows(2).all(|w| w[0].score >= w[1].score),
        "a batch must be descending: {:?}",
        batch.items.iter().map(|i| i.score).collect::<Vec<_>>()
    );
    // `last_activity` is `NOW - id`, so the lowest ids are newest and win the
    // recency term.
    assert_eq!(batch.items[0].primary_text, "acme item 1");
}

/// The prefilter is the finder's headline latency mechanism, and it is only
/// observable through the stats — so this is the test that would fail if it
/// were quietly removed.
#[test]
fn the_prefilter_keeps_the_aligner_off_almost_every_entry() {
    let rows: Vec<(ItemKind, i64, String, String)> = (0..2_000)
        .map(|id| {
            let text = if id % 100 == 0 {
                format!("quarterly report {id}")
            } else {
                format!("lunch {id}")
            };
            (ItemKind::Message, id, text, String::new())
        })
        .collect();
    let borrowed: Vec<(ItemKind, i64, &str, &str)> = rows
        .iter()
        .map(|(k, id, p, s)| (*k, *id, p.as_str(), s.as_str()))
        .collect();
    let store = store_of(&borrowed);

    let batch = final_batch(&store, &query("qrtly"));
    let ScanStats {
        scanned, aligned, ..
    } = batch.stats;
    assert_eq!(scanned, 2_000, "every entry is walked");
    assert!(
        aligned < scanned / 10,
        "the prefilter must reject the overwhelming majority: aligned {aligned} of {scanned}"
    );
    assert!(aligned >= batch.stats.matched, "every match was aligned");
    assert!(batch.stats.matched > 0, "the query does match something");
}

/// ...and it must never reject something the aligner would have matched.
#[test]
fn the_prefilter_never_loses_a_real_match() {
    let store = store_of(&[
        (ItemKind::Message, 1, "Café meeting", ""),
        (ItemKind::Message, 2, "ﬁle a bug", ""),
        (ItemKind::Message, 3, "会議の議事録", ""),
        (ItemKind::Message, 4, "Quarterly Report", ""),
    ]);
    for (needle, expected) in [
        ("cafe", "Café meeting"),
        ("file", "ﬁle a bug"),
        ("会議", "会議の議事録"),
        ("qr", "Quarterly Report"),
    ] {
        let batch = final_batch(&store, &query(needle));
        assert!(
            texts(&batch).contains(&expected),
            "{needle:?} lost {expected:?}: got {:?}",
            texts(&batch)
        );
    }
}

// ---------------------------------------------------------------------------
// batching and cancellation
// ---------------------------------------------------------------------------

/// prd.md: "flushing descending batches ... ~every 2k candidates". A scan
/// large enough must paint before it finishes.
#[test]
fn a_large_scan_flushes_partial_batches_before_it_completes() {
    let rows: Vec<(ItemKind, i64, String, String)> = (0..BATCH_STRIDE as i64 * 3)
        .map(|id| {
            (
                ItemKind::Message,
                id,
                format!("acme item {id}"),
                String::new(),
            )
        })
        .collect();
    let borrowed: Vec<(ItemKind, i64, &str, &str)> = rows
        .iter()
        .map(|(k, id, p, s)| (*k, *id, p.as_str(), s.as_str()))
        .collect();
    let store = store_of(&borrowed);

    let batches = run(&store, &query("acme"));
    assert!(
        batches.len() > 1,
        "a scan of {} entries must flush before it ends",
        rows.len()
    );
    assert!(batches.len() <= MAX_INTERMEDIATE_BATCHES + 1);
    assert!(
        batches[..batches.len() - 1].iter().all(|b| !b.complete),
        "only the last batch is complete"
    );
    let last = batches.last().expect("non-empty");
    assert!(last.complete);
    // Every batch is internally descending — the property a renderer relies
    // on when it draws one without re-sorting.
    for batch in &batches {
        assert!(batch.items.windows(2).all(|w| w[0].score >= w[1].score));
    }
}

/// A small scan should not pay for intermediate renders at all.
#[test]
fn a_small_scan_emits_exactly_one_batch() {
    let store = store_of(&[(ItemKind::Message, 1, "acme", "")]);
    let batches = run(&store, &query("acme"));
    assert_eq!(batches.len(), 1);
    assert!(batches[0].complete);
}

/// The keystroke-cancellation property: a superseded query must *stop*, not
/// run to completion and have its output discarded.
#[test]
fn a_cancelled_scan_stops_early() {
    let rows: Vec<(ItemKind, i64, String, String)> = (0..50_000)
        .map(|id| (ItemKind::Message, id, format!("acme {id}"), String::new()))
        .collect();
    let borrowed: Vec<(ItemKind, i64, &str, &str)> = rows
        .iter()
        .map(|(k, id, p, s)| (*k, *id, p.as_str(), s.as_str()))
        .collect();
    let store = store_of(&borrowed);

    let cancel = CancellationToken::new();
    cancel.cancel();
    let batches = run_with(&store, &query("acme"), &cancel, &FinderRanking::default());
    let last = batches.last().expect("a final batch is always sent");
    assert!(last.stats.cancelled, "the scan must report that it stopped");
    assert!(
        last.stats.scanned < rows.len() as u64,
        "a cancelled scan walked all {} entries",
        rows.len()
    );
    // ...and it still closes the stream, so a client can tell "stopped" from
    // "still running".
    assert!(last.complete);
}

/// A sink that stops asking must stop the scan — a client that hung up costs
/// nothing further.
#[test]
fn a_sink_that_breaks_stops_the_scan() {
    let rows: Vec<(ItemKind, i64, String, String)> = (0..BATCH_STRIDE as i64 * 4)
        .map(|id| (ItemKind::Message, id, format!("acme {id}"), String::new()))
        .collect();
    let borrowed: Vec<(ItemKind, i64, &str, &str)> = rows
        .iter()
        .map(|(k, id, p, s)| (*k, *id, p.as_str(), s.as_str()))
        .collect();
    let store = store_of(&borrowed);

    let mut seen = 0usize;
    let mut sink = |_batch: Batch| {
        seen += 1;
        ControlFlow::Break(())
    };
    scan(
        &store,
        &query("acme"),
        &FinderRanking::default(),
        NOW,
        &CancellationToken::new(),
        &mut sink,
    );
    assert_eq!(seen, 1, "the scan kept going after the sink hung up");
}

// ---------------------------------------------------------------------------
// the async surface
// ---------------------------------------------------------------------------

fn finder_over(store: FinderStore) -> Finder {
    Finder::new(Arc::new(RwLock::new(store)), &FinderConfig::default())
}

#[tokio::test]
async fn find_returns_the_final_page() {
    let finder = finder_over(store_of(&[
        (ItemKind::Message, 1, "Acme invoice", ""),
        (ItemKind::Message, 2, "Lunch", ""),
    ]));
    let found = finder
        .find(query("acme"), CancellationToken::new())
        .await
        .expect("scan runs");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].primary_text, "Acme invoice");
}

#[tokio::test]
async fn find_batched_delivers_a_complete_batch() {
    let finder = finder_over(store_of(&[(ItemKind::Message, 1, "Acme invoice", "")]));
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let stats = finder
        .find_batched(query("acme"), CancellationToken::new(), tx)
        .await
        .expect("scan runs");
    assert_eq!(stats.scanned, 1);
    let batch = rx.recv().await.expect("a batch arrives");
    assert!(batch.complete);
    assert_eq!(batch.items.len(), 1);
    assert!(rx.recv().await.is_none(), "the stream ends");
}

/// Dropping the receiver stops the scan.
///
/// This is the mechanism `finder_service`'s `Find` relies on to stop working
/// for a client that hung up: its forwarder *owns* the batch receiver, so
/// returning drops it, and the scan's next `blocking_send` fails. If the scan
/// ignored a closed channel it would walk the whole store — holding
/// `FinderStore`'s read lock, which the drain needs to write — for a client
/// that is already gone.
#[tokio::test]
async fn a_dropped_receiver_stops_the_scan() {
    let rows: Vec<(ItemKind, i64, String, String)> = (0..80_000)
        .map(|id| (ItemKind::Message, id, format!("acme {id}"), String::new()))
        .collect();
    let borrowed: Vec<(ItemKind, i64, &str, &str)> = rows
        .iter()
        .map(|(k, id, p, s)| (*k, *id, p.as_str(), s.as_str()))
        .collect();
    let finder = finder_over(store_of(&borrowed));

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    drop(rx);
    let stats = finder
        .find_batched(query("acme"), CancellationToken::new(), tx)
        .await
        .expect("scan runs");
    assert!(
        stats.scanned < rows.len() as u64,
        "the scan walked all {} entries for a receiver that was already gone",
        rows.len()
    );
}

#[test]
fn the_limit_is_clamped_to_the_configured_maximum() {
    let finder = finder_over(FinderStore::new());
    assert_eq!(finder.clamp_limit(0), finder.max_results());
    assert_eq!(finder.clamp_limit(5), 5);
    assert_eq!(finder.clamp_limit(usize::MAX), finder.max_results());
}
