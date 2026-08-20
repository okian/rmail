//! Tests for the two queries task 91's band is built on.
//!
//! `panic!` in a match arm that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `keymap::tests` takes.
#![allow(clippy::panic)]

use super::*;
use crate::keymap::{Pending, Resolution};

fn chord(text: &str) -> Chord {
    match Chord::parse(text) {
        Ok(chord) => chord,
        Err(error) => panic!("{text:?} should parse: {error}"),
    }
}

/// Bind `chord` in `mode`, going through the public path so a fixture cannot
/// build a map the engine would have refused.
fn bind(keymap: &mut Keymap, mode: Mode, text: &str, action: Action) {
    match keymap.bind(mode, chord(text), action) {
        Ok(()) => {}
        Err(error) => panic!("binding {text:?} in {} failed: {error}", mode.id()),
    }
}

fn keys(text: &str) -> Vec<Key> {
    chord(text).keys().to_vec()
}

/// The keys a list of continuations offers, in order.
fn offered(found: &[Continuation]) -> Vec<String> {
    found.iter().map(|c| c.key.to_string()).collect()
}

// ---------------------------------------------------------------------------
// continuations
// ---------------------------------------------------------------------------

#[test]
fn the_default_map_offers_g_after_a_pending_g() {
    let keymap = Keymap::defaults();
    let found = keymap.continuations(Mode::Normal, &keys("g"));
    // Every chord under `g`, each leading somewhere. Asserted by name rather
    // than by count, because the set grows — task 101 added `gs` — and a count
    // was really asserting how many chords this build binds under `g`.
    let offered = offered(&found);
    for key in ["g", "s"] {
        assert!(offered.contains(&key.to_owned()), "{offered:?}");
    }
    let leads = |key: &str| {
        found
            .iter()
            .find(|c| c.key.to_string() == key)
            .map(|c| c.leads.clone())
    };
    match leads("g") {
        Some(Leads::Run(action)) => assert_eq!(action, Action::CursorTop),
        other => panic!("expected `gg` to complete a binding, found {other:?}"),
    }
    match leads("s") {
        Some(Leads::Run(action)) => assert_eq!(action, Action::SettingsOpen),
        other => panic!("expected `gs` to complete a binding, found {other:?}"),
    }
    assert!(
        found.iter().all(|c| c.buried.is_empty()),
        "nothing longer than two keys is bound under `g`, so nothing is dead"
    );
}

#[test]
fn a_prefix_of_several_bindings_is_a_group_labelled_by_their_common_id() {
    let mut keymap = Keymap::defaults();
    bind(&mut keymap, Mode::Normal, " a", Action::AiPanel);
    bind(&mut keymap, Mode::Normal, " q", Action::AiQuick);
    let found = keymap.continuations(Mode::Normal, &[]);
    let space = found
        .iter()
        .find(|c| c.key == Key::Char(' '))
        .cloned()
        .unwrap_or_else(|| panic!("expected a continuation for `<space>`: {found:?}"));
    match space.leads {
        Leads::Group { label, members } => {
            assert_eq!(
                label, "ai",
                "the label is the longest common dot-prefix of the member ids, \
                 never a hand-written group name"
            );
            assert_eq!(members, 2);
        }
        other => panic!("expected a group, found {other:?}"),
    }
}

#[test]
fn a_group_whose_members_share_no_leading_segment_is_left_unlabelled() {
    // `help` and `search` have no common segment, and inventing a name for an
    // arbitrary collection is exactly the hand-maintained table this
    // derivation replaces. A renderer shows the member count instead.
    let mut keymap = Keymap::defaults();
    bind(&mut keymap, Mode::Normal, " h", Action::Help);
    bind(&mut keymap, Mode::Normal, " s", Action::SearchOpen);
    let found = keymap.continuations(Mode::Normal, &[]);
    let space = found
        .iter()
        .find(|c| c.key == Key::Char(' '))
        .cloned()
        .unwrap_or_else(|| panic!("expected a continuation for `<space>`: {found:?}"));
    match space.leads {
        Leads::Group { label, members } => {
            assert!(label.is_empty(), "{label:?}");
            assert_eq!(members, 2);
        }
        other => panic!("expected a group, found {other:?}"),
    }
}

#[test]
fn a_label_is_segments_rather_than_characters() {
    // `manual.back` and `menu.accept` share the letter `m` and no segment.
    // Character-wise prefixing would answer `m`, which names nothing.
    let mut keymap = Keymap::defaults();
    bind(&mut keymap, Mode::Menu, " b", Action::ManualBack);
    bind(&mut keymap, Mode::Menu, " a", Action::MenuAccept);
    let found = keymap.continuations(Mode::Menu, &[]);
    let space = found
        .iter()
        .find(|c| c.key == Key::Char(' '))
        .cloned()
        .unwrap_or_else(|| panic!("expected a continuation for `<space>`: {found:?}"));
    match space.leads {
        Leads::Group { label, .. } => assert!(label.is_empty(), "{label:?}"),
        other => panic!("expected a group, found {other:?}"),
    }
}

#[test]
fn a_one_member_group_is_labelled_by_that_members_whole_id() {
    let mut keymap = Keymap::defaults();
    bind(&mut keymap, Mode::Normal, " x", Action::Archive);
    let found = keymap.continuations(Mode::Normal, &[]);
    let space = found
        .iter()
        .find(|c| c.key == Key::Char(' '))
        .cloned()
        .unwrap_or_else(|| panic!("expected a continuation for `<space>`: {found:?}"));
    match space.leads {
        Leads::Group { label, members } => {
            assert_eq!(label, "message.archive");
            assert_eq!(members, 1);
        }
        other => panic!("expected a group, found {other:?}"),
    }
}

#[test]
fn continuations_walk_the_whole_chain_not_just_the_nearest_layer() {
    // `Visual` inherits `Normal`, so a `g` pending in Visual has to offer
    // `Normal`'s `gg` — a band that showed only the nearest layer would say a
    // key does nothing while the engine runs it.
    let keymap = Keymap::defaults();
    // `contains`, not equality: `Normal`'s `g` prefix has grown a second
    // continuation (`gs`, task 101), and asserting the whole list was asserting
    // how many chords this build binds under `g` rather than that the chain was
    // walked at all.
    assert!(offered(&keymap.continuations(Mode::Visual, &keys("g"))).contains(&"g".to_owned()));
    assert!(offered(&keymap.continuations(Mode::Viewer, &keys("g"))).contains(&"g".to_owned()));
}

#[test]
fn a_mode_whose_chain_stops_at_global_offers_nothing_from_normal() {
    // `Pick` chains to `Global` only. `Normal`'s `gg` must not appear there,
    // for the reason the chain stops: a key reaching the list behind a modal
    // is the bug the layering exists to prevent.
    let mut keymap = Keymap::defaults();
    // Give `Pick` a `g` prefix of its own so the question is "whose `gg`",
    // not "is `g` a prefix at all".
    keymap.unbind(Mode::Pick, &chord("gg"));
    assert!(
        keymap.continuations(Mode::Pick, &keys("g")).is_empty(),
        "Pick has no `g` chords of its own once `gg` is removed"
    );
}

#[test]
fn an_empty_prefix_lists_every_first_key_in_the_mode() {
    let keymap = Keymap::defaults();
    let found = keymap.continuations(Mode::Normal, &[]);
    let offered = offered(&found);
    for expected in ["j", "k", "a", "d", ":", "<esc>", "<c-c>"] {
        assert!(
            offered.contains(&expected.to_owned()),
            "{expected} is bound in Normal's chain: {offered:?}"
        );
    }
    // `g` is a group (only `gg` is under it); `j` completes on its own.
    let group = found.iter().find(|c| c.key == Key::Char('g'));
    assert!(
        matches!(group.map(|c| &c.leads), Some(Leads::Group { .. })),
        "{group:?}"
    );
}

#[test]
fn a_prefix_nothing_extends_has_no_continuations() {
    let keymap = Keymap::defaults();
    assert!(keymap.continuations(Mode::Normal, &keys("j")).is_empty());
    assert!(keymap.continuations(Mode::Normal, &keys("zz")).is_empty());
}

#[test]
fn a_chord_bound_in_two_layers_of_one_chain_counts_once() {
    // `Visual` and `Normal` both reachable from Visual; binding the same chord
    // in both is one way to press it, not two members of a group.
    let mut keymap = Keymap::defaults();
    bind(&mut keymap, Mode::Normal, " a", Action::AiPanel);
    bind(&mut keymap, Mode::Visual, " a", Action::AiQuick);
    let found = keymap.continuations(Mode::Visual, &[]);
    let space = found
        .iter()
        .find(|c| c.key == Key::Char(' '))
        .cloned()
        .unwrap_or_else(|| panic!("expected a continuation for `<space>`: {found:?}"));
    match space.leads {
        Leads::Group { members, .. } => assert_eq!(members, 1, "{space:?}"),
        other => panic!("expected a group, found {other:?}"),
    }
}

#[test]
fn a_continuation_that_completes_a_chord_buries_the_longer_ones_under_it() {
    // The cross-layer case `bind` cannot refuse: `Normal` has the three-key
    // `<space>ab`, and `Visual` binds the two-key `<space>a`. In Visual, `a`
    // after `<space>` runs Visual's binding and nothing ever waits for `b`.
    let mut keymap = Keymap::defaults();
    bind(&mut keymap, Mode::Normal, " ab", Action::AiPanel);
    bind(&mut keymap, Mode::Visual, " a", Action::AiQuick);

    let found = keymap.continuations(Mode::Visual, &keys(" "));
    let a = found
        .iter()
        .find(|c| c.key == Key::Char('a'))
        .cloned()
        .unwrap_or_else(|| panic!("expected a continuation for `a`: {found:?}"));
    assert_eq!(a.leads, Leads::Run(Action::AiQuick));
    assert_eq!(
        a.buried
            .iter()
            .map(|(chord, action)| format!("{chord} -> {}", action.id()))
            .collect::<Vec<_>>(),
        ["<space>ab -> ai.panel"],
        "the band has to say so: a binding that silently does nothing is what \
         this field exists to surface"
    );

    // And in Normal, where nothing shadows it, the same key is a group.
    let found = keymap.continuations(Mode::Normal, &keys(" "));
    let a = found
        .iter()
        .find(|c| c.key == Key::Char('a'))
        .cloned()
        .unwrap_or_else(|| panic!("expected a continuation for `a`: {found:?}"));
    assert!(matches!(a.leads, Leads::Group { .. }), "{a:?}");
    assert!(a.buried.is_empty(), "{a:?}");
}

// ---------------------------------------------------------------------------
// the band needs no timer, and this is why
// ---------------------------------------------------------------------------

/// The proof task 91's acceptance asks for: a pending prefix is always one
/// that already resolved to nothing.
///
/// vim delays because an exact match that is also a prefix is ambiguous there.
/// Here rule 1 fires an exact match immediately, so for every mode and every
/// prefix that [`Keymap::resolve`] leaves pending, `lookup` on that prefix
/// returned `None` — nothing half-typed could have fired on its own, so there
/// is nothing for a delay to disambiguate.
///
/// Asserted over every prefix of every binding in every configurable mode, so
/// it is a statement about the engine rather than about one chord.
#[test]
fn a_pending_prefix_is_always_one_that_resolved_to_nothing() {
    let keymap = Keymap::defaults();
    let mut checked = 0;
    for mode in Mode::CONFIGURABLE {
        let mut chords: Vec<Chord> = Vec::new();
        for layer in mode.chain() {
            for (chord, _) in keymap.layer(*layer) {
                chords.push(chord.clone());
            }
        }
        for chord in chords {
            for len in 1..chord.keys().len() {
                let prefix: Vec<Key> = chord.keys().iter().copied().take(len).collect();
                let mut pending = Pending::default();
                let mut last = None;
                for key in &prefix {
                    last = Some(keymap.resolve(*mode, &mut pending, *key));
                }
                // Only prefixes the engine actually holds are the band's
                // subject: one that resolved to something is not pending at
                // all, which is the other half of the same claim.
                if last != Some(Resolution::Pending) {
                    continue;
                }
                checked += 1;
                assert_eq!(
                    pending.keys(),
                    prefix.as_slice(),
                    "the engine holds exactly what was typed"
                );
                let held = match Chord::new(prefix.clone()) {
                    Ok(chord) => chord,
                    Err(error) => panic!("a held prefix is a chord: {error}"),
                };
                assert_eq!(
                    keymap.lookup(*mode, &held),
                    None,
                    "{held} is pending in {} and yet something is bound to it — \
                     the band would need a timer to know whether that binding \
                     was going to fire",
                    mode.id()
                );
                assert!(
                    !keymap.continuations(*mode, &prefix).is_empty(),
                    "{held} is pending in {}, so something must extend it",
                    mode.id()
                );
            }
        }
    }
    assert!(
        checked > 0,
        "the default map has prefixes (`gg`), so this proved something"
    );
}

// ---------------------------------------------------------------------------
// cross-layer shadowing
// ---------------------------------------------------------------------------

#[test]
fn the_default_bindings_shadow_nothing_across_their_layers() {
    assert_eq!(
        Keymap::defaults().shadowed_across_layers(),
        Vec::new(),
        "a built-in binding no keyboard can deliver would be a defect, and \
         `defaults` installs through `insert` rather than `bind` so this is \
         the only check that covers it"
    );
}

#[test]
fn a_shorter_chord_in_a_nearer_layer_kills_the_longer_one_it_prefixes() {
    let mut keymap = Keymap::defaults();
    // Legal, and reasonable to want: `g` in the viewer. It also means
    // `Normal`'s `gg` can never be typed while the viewer is up.
    bind(&mut keymap, Mode::Viewer, "g", Action::AiPanel);
    let found = keymap.shadowed_across_layers();
    let mut reported: Vec<(String, String, String)> = found
        .iter()
        .map(|(mode, dead, killer)| (mode.id().to_owned(), dead.to_string(), killer.to_string()))
        .collect();
    reported.sort();
    // Every chord `g` kills, not one of them: `Normal` binds `gg` and — since
    // task 101 — `gs`, and both become untypeable in the viewer the moment `g`
    // fires there. A report naming only the first would understate what the
    // binding cost.
    assert_eq!(
        reported,
        [
            ("viewer".to_owned(), "gg".to_owned(), "g".to_owned()),
            ("viewer".to_owned(), "gs".to_owned(), "g".to_owned()),
        ],
        "reported for the mode somebody meets it in, naming each dead binding \
         and the one that fires instead"
    );
}

#[test]
fn a_shadow_is_reported_for_every_mode_whose_chain_has_it() {
    // Bound in `Normal`, so `Viewer` and `Visual` inherit both the killer and
    // the victim — three modes, one edit.
    let mut keymap = Keymap::defaults();
    keymap.unbind(Mode::Normal, &chord("gg"));
    bind(&mut keymap, Mode::Normal, "gab", Action::AiPanel);
    bind(&mut keymap, Mode::Visual, "ga", Action::AiQuick);
    let modes: Vec<&str> = keymap
        .shadowed_across_layers()
        .iter()
        .map(|(mode, _, _)| mode.id())
        .collect();
    assert_eq!(
        modes,
        ["visual"],
        "only `Visual`'s chain has both, because that is where `ga` is bound"
    );
}

#[test]
fn only_the_killer_that_actually_fires_is_reported() {
    // `a`, `ab` and `abc` all bound across a chain: `a` fires, so `ab` and
    // `abc` are both dead — each reported once, against `a`, rather than
    // `abc` being reported twice against both of its bound prefixes.
    let mut keymap = Keymap::defaults();
    bind(&mut keymap, Mode::Normal, "zab", Action::AiPanel);
    bind(&mut keymap, Mode::Viewer, "za", Action::AiQuick);
    bind(&mut keymap, Mode::Visual, "z", Action::Help);
    let viewer: Vec<(String, String)> = keymap
        .shadowed_across_layers()
        .iter()
        .filter(|(mode, _, _)| *mode == Mode::Visual)
        .map(|(_, dead, killer)| (dead.to_string(), killer.to_string()))
        .collect();
    assert_eq!(
        viewer,
        [("zab".to_owned(), "z".to_owned())],
        "`za` is bound in Viewer, which Visual's chain does not include, so \
         Visual sees only `zab` — and against `z`, the binding that fires"
    );
}

#[test]
fn a_shadow_the_engine_agrees_with() {
    // The report is only worth having if it matches what `resolve` does, so
    // this asserts the behaviour rather than the report: the dead chord's
    // first key runs the killer, and the second key is a separate press.
    let mut keymap = Keymap::defaults();
    bind(&mut keymap, Mode::Viewer, "g", Action::AiPanel);
    let mut pending = Pending::default();
    assert_eq!(
        keymap.resolve(Mode::Viewer, &mut pending, Key::Char('g')),
        Resolution::Run {
            action: Action::AiPanel,
            count: None,
        },
        "the shorter chord fires at once — rule 1, and the reason no timer is \
         needed anywhere here"
    );
    assert!(pending.is_empty(), "so nothing is left waiting for `gg`");
}
