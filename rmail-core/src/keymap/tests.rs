//! Tests for the keymap engine.
//!
//! These drive [`Keymap::resolve`] directly — no model, no terminal. What
//! they are protecting is mostly *negative*: that a half-typed chord cannot
//! eat the key after it, that no mode can trap the user, and that nothing the
//! engine accumulates between keystrokes grows with how long a key is held.
//!
//! `panic!` in a match arm that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tag_cli::tests` takes, for the same reason (`clippy.toml` carves out
//! `unwrap`/`expect` in tests but not `panic`).
#![allow(clippy::panic)]

use super::*;

/// Feed a string of ordinary characters and collect what each one resolved
/// to, keeping the pending state across them the way the model does.
fn feed(keymap: &Keymap, mode: Mode, pending: &mut Pending, keys: &str) -> Vec<Resolution> {
    keys.chars()
        .map(|c| keymap.resolve(mode, pending, Key::Char(c)))
        .collect()
}

/// What one key press resolves to, starting from nothing pending.
fn once(keymap: &Keymap, mode: Mode, key: Key) -> Resolution {
    keymap.resolve(mode, &mut Pending::default(), key)
}

fn run(action: Action) -> Resolution {
    Resolution::Run {
        action,
        count: None,
    }
}

fn chord(text: &str) -> Chord {
    match Chord::parse(text) {
        Ok(chord) => chord,
        Err(error) => panic!("{text:?} should parse: {error}"),
    }
}

// ---------------------------------------------------------------------------
// chords
// ---------------------------------------------------------------------------

#[test]
fn chords_round_trip_through_vim_notation() {
    for (text, keys) in [
        ("j", vec![Key::Char('j')]),
        ("gg", vec![Key::Char('g'), Key::Char('g')]),
        ("<esc>", vec![Key::Esc]),
        ("<c-p>", vec![Key::Ctrl('p')]),
        ("<enter>", vec![Key::Enter]),
        ("<tab>", vec![Key::Tab]),
        ("<bs>", vec![Key::Backspace]),
        ("<up>", vec![Key::Up]),
        ("<down>", vec![Key::Down]),
        ("<space>", vec![Key::Char(' ')]),
        ("<lt>", vec![Key::Char('<')]),
        ("g<enter>", vec![Key::Char('g'), Key::Enter]),
    ] {
        let parsed = chord(text);
        assert_eq!(parsed.keys(), keys, "{text} parsed wrong");
        assert_eq!(
            parsed.to_string(),
            text,
            "{text} did not come back out the way it went in"
        );
    }
}

#[test]
fn key_names_have_the_aliases_a_config_file_needs() {
    // vim spells them `<cr>`/`<bs>`; the key caps say Enter and Backspace.
    // A config file should not be a spelling test.
    assert_eq!(chord("<cr>"), chord("<enter>"));
    assert_eq!(chord("<return>"), chord("<enter>"));
    assert_eq!(chord("<escape>"), chord("<esc>"));
    assert_eq!(chord("<backspace>"), chord("<bs>"));
    assert_eq!(chord("<C-P>"), chord("<c-p>"), "case-insensitive inside <>");
}

#[test]
fn a_chord_that_is_not_one_says_which_part_is_wrong() {
    for (text, expected) in [
        ("", "at least one key"),
        ("<esc", "unterminated"),
        ("<nope>", "unknown key"),
        ("<c-shift-p>", "unknown key"),
        ("jjjjj", "longer than the 4-key limit"),
    ] {
        let error = match Chord::parse(text) {
            Ok(chord) => panic!("{text:?} should not have parsed, got {chord}"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains(expected),
            "{text:?} reported {error:?}, which does not mention {expected:?}"
        );
    }
}

#[test]
fn a_pathologically_long_chord_is_refused_without_being_parsed_in_full() {
    // The bound is on the parser, not only on the result: a `keys.toml` line
    // of a million `j`s should cost five keys of work, not a million.
    let error = Chord::parse(&"j".repeat(1_000_000));
    assert!(error.is_err(), "a 1e6-key chord was accepted");
}

// ---------------------------------------------------------------------------
// the action registry
// ---------------------------------------------------------------------------

#[test]
fn every_action_has_a_unique_id_that_round_trips() {
    let mut seen = std::collections::BTreeSet::new();
    for action in Action::ALL {
        assert!(
            seen.insert(action.id()),
            "two actions share the id {:?}",
            action.id()
        );
        assert_eq!(
            Action::from_id(action.id()),
            Some(*action),
            "{} does not parse back",
            action.id()
        );
        assert!(!action.describe().is_empty(), "{} has no help", action.id());
    }
    assert_eq!(Action::from_id("no.such.action"), None);
}

#[test]
fn defaults_are_all_installable() {
    // `Keymap::defaults` cannot fail by contract — it logs and skips a
    // binding that does not parse, because the TUI has to start whatever else
    // is broken. This is what keeps that log line unreachable rather than
    // merely unlikely: every default must survive the trip.
    let keymap = Keymap::defaults();
    // Derived from `CONFIGURABLE` rather than restated: a hand-written mode
    // list here silently *under*-counts when a mode is added (task 85 added
    // two), so this assertion would have kept passing while the new layers
    // went unchecked.
    let installed: usize = std::iter::once(&Mode::Global)
        .chain(Mode::CONFIGURABLE)
        .map(|mode| keymap.layer(*mode).count())
        .sum();
    assert_eq!(
        installed,
        DEFAULTS.len(),
        "a built-in binding was silently dropped"
    );
}

// ---------------------------------------------------------------------------
// resolution
// ---------------------------------------------------------------------------

#[test]
fn a_bound_key_resolves_to_its_action() {
    let keymap = Keymap::defaults();
    assert_eq!(
        once(&keymap, Mode::Normal, Key::Char('j')),
        run(Action::CursorDown)
    );
    assert_eq!(
        once(&keymap, Mode::Normal, Key::Down),
        run(Action::CursorDown)
    );
    assert_eq!(
        once(&keymap, Mode::Normal, Key::Char('a')),
        run(Action::Archive)
    );
}

#[test]
fn a_chord_waits_for_its_second_key_and_then_fires() {
    let keymap = Keymap::defaults();
    let mut pending = Pending::default();

    assert_eq!(
        keymap.resolve(Mode::Normal, &mut pending, Key::Char('g')),
        Resolution::Pending
    );
    assert_eq!(pending.keys(), [Key::Char('g')], "the g is held");
    assert_eq!(
        keymap.resolve(Mode::Normal, &mut pending, Key::Char('g')),
        run(Action::CursorTop)
    );
    assert!(pending.is_empty(), "the chord is consumed, not left behind");
}

#[test]
fn a_dead_sequence_retries_its_tail_instead_of_eating_it() {
    // The rule task 83 established and this engine has to keep: a half-typed
    // `g` costs the `g`, never the key that followed it. vim discards both;
    // silently eating a keystroke is a bug wearing a feature's clothes.
    let keymap = Keymap::defaults();
    let mut pending = Pending::default();

    assert_eq!(
        feed(&keymap, Mode::Normal, &mut pending, "gk"),
        vec![Resolution::Pending, run(Action::CursorUp)],
    );
    assert!(pending.is_empty());

    assert_eq!(
        feed(&keymap, Mode::Normal, &mut pending, "gq"),
        vec![Resolution::Pending, run(Action::Back)],
    );
}

#[test]
fn a_partial_chord_does_not_survive_to_pair_with_a_later_key() {
    let keymap = Keymap::defaults();
    let mut pending = Pending::default();
    let resolutions = feed(&keymap, Mode::Normal, &mut pending, "gjg");
    assert_eq!(
        resolutions,
        vec![
            Resolution::Pending,
            run(Action::CursorDown),
            Resolution::Pending
        ],
        "the first g was spent on the j, so the third key starts fresh"
    );
}

#[test]
fn an_unbound_key_is_reported_rather_than_swallowed() {
    let keymap = Keymap::defaults();
    let mut pending = Pending::default();
    assert_eq!(
        feed(&keymap, Mode::Normal, &mut pending, "\u{1}zj"),
        vec![
            Resolution::Unbound(Key::Char('\u{1}')),
            Resolution::Unbound(Key::Char('z')),
            run(Action::CursorDown),
        ],
        "an unbound key must not put the engine into a state that eats the next one"
    );
    assert!(pending.is_empty());
}

#[test]
fn a_chord_that_dies_at_the_length_limit_still_reports_its_last_key() {
    let mut keymap = Keymap::empty();
    assert!(keymap
        .bind(Mode::Normal, chord("zzzz"), Action::Quit)
        .is_ok());
    let mut pending = Pending::default();

    let resolutions = feed(&keymap, Mode::Normal, &mut pending, "zzzy");
    assert_eq!(
        resolutions.last(),
        Some(&Resolution::Unbound(Key::Char('y'))),
        "the key that killed the sequence is still handed back: {resolutions:?}"
    );
    assert!(pending.is_empty());
}

#[test]
fn an_exact_match_wins_over_waiting_for_a_longer_one() {
    // `update` has no clock, so there is no `timeoutlen` to wait out. A
    // keymap that somehow holds both (only reachable by building one by hand
    // — `bind` refuses the pair) must still resolve deterministically now
    // rather than hang on a key that never comes.
    let mut keymap = Keymap::empty();
    assert!(keymap.bind(Mode::Normal, chord("g"), Action::Quit).is_ok());
    let conflict = keymap.bind(Mode::Normal, chord("gg"), Action::CursorTop);
    assert!(conflict.is_err(), "the shadowing pair should be refused");

    assert_eq!(
        once(&keymap, Mode::Normal, Key::Char('g')),
        run(Action::Quit)
    );
}

// ---------------------------------------------------------------------------
// counts
// ---------------------------------------------------------------------------

#[test]
fn a_count_is_collected_and_handed_to_the_action() {
    let keymap = Keymap::defaults();
    let mut pending = Pending::default();
    let resolutions = feed(&keymap, Mode::Normal, &mut pending, "12j");
    assert_eq!(
        resolutions,
        vec![
            Resolution::Pending,
            Resolution::Pending,
            Resolution::Run {
                action: Action::CursorDown,
                count: Some(12)
            }
        ]
    );
    assert!(pending.is_empty(), "the count is consumed with the action");
}

#[test]
fn a_count_survives_across_a_chord_and_shows_in_the_status_line() {
    let keymap = Keymap::defaults();
    let mut pending = Pending::default();
    feed(&keymap, Mode::Normal, &mut pending, "3g");
    assert_eq!(
        pending.label(),
        "3g",
        "a half-typed command has to be visible, or it is indistinguishable \
         from a keyboard that stopped responding"
    );
    assert_eq!(
        keymap.resolve(Mode::Normal, &mut pending, Key::Char('g')),
        Resolution::Run {
            action: Action::CursorTop,
            count: Some(3)
        }
    );
}

#[test]
fn a_leading_zero_is_a_key_and_not_a_count() {
    let keymap = Keymap::defaults();
    let mut pending = Pending::default();
    // Nothing binds `0` today, so it comes back unbound rather than starting
    // a count of nothing — which is what leaves `0` free to be bound.
    assert_eq!(
        keymap.resolve(Mode::Normal, &mut pending, Key::Char('0')),
        Resolution::Unbound(Key::Char('0'))
    );
    // But a zero *after* a digit is arithmetic.
    assert_eq!(
        feed(&keymap, Mode::Normal, &mut pending, "10j").last(),
        Some(&Resolution::Run {
            action: Action::CursorDown,
            count: Some(10)
        })
    );
}

#[test]
fn a_held_down_digit_saturates_instead_of_growing() {
    // The bound that matters: a user leaning on `9` must not be able to make
    // the engine accumulate. The count stops at MAX_COUNT and the pending
    // chord never takes a key at all.
    let keymap = Keymap::defaults();
    let mut pending = Pending::default();
    for _ in 0..10_000 {
        assert_eq!(
            keymap.resolve(Mode::Normal, &mut pending, Key::Char('9')),
            Resolution::Pending
        );
        assert!(
            pending.count().is_some_and(|count| count <= MAX_COUNT),
            "the count ran past {MAX_COUNT}: {:?}",
            pending.count()
        );
        assert!(pending.keys().is_empty(), "digits are not chord keys");
        assert!(
            pending.label().len() <= 5,
            "the status label grew with the key being held: {:?}",
            pending.label()
        );
    }
    assert_eq!(pending.count(), Some(MAX_COUNT));
}

#[test]
fn a_held_down_chord_prefix_never_grows_the_pending_buffer() {
    let mut keymap = Keymap::empty();
    assert!(keymap
        .bind(Mode::Normal, chord("zzzz"), Action::Quit)
        .is_ok());
    let mut pending = Pending::default();
    for _ in 0..10_000 {
        keymap.resolve(Mode::Normal, &mut pending, Key::Char('z'));
        assert!(
            pending.keys().len() < MAX_CHORD_KEYS,
            "the pending chord reached {} keys",
            pending.keys().len()
        );
    }
}

#[test]
fn insert_mode_treats_digits_as_text() {
    // The whole reason counts are a per-mode property: an address with a 3 in
    // it must not become a repeat count, and the 3 must not disappear.
    let keymap = Keymap::defaults();
    let mut pending = Pending::default();
    assert_eq!(
        feed(&keymap, Mode::Insert, &mut pending, "3"),
        vec![Resolution::Unbound(Key::Char('3'))]
    );
    assert!(pending.is_empty());
}

// ---------------------------------------------------------------------------
// the way out
// ---------------------------------------------------------------------------

#[test]
fn esc_and_ctrl_c_resolve_in_every_mode() {
    let keymap = Keymap::defaults();
    for mode in [
        Mode::Global,
        Mode::Normal,
        Mode::Viewer,
        Mode::Visual,
        Mode::Insert,
        Mode::Pick,
        Mode::Confirm,
        Mode::Help,
    ] {
        assert_eq!(
            once(&keymap, mode, Key::Esc),
            run(Action::Cancel),
            "Esc does not escape {} mode",
            mode.id()
        );
        assert_eq!(
            once(&keymap, mode, Key::CTRL_C),
            run(Action::Quit),
            "Ctrl-C does not quit from {} mode",
            mode.id()
        );
    }
}

#[test]
fn esc_and_ctrl_c_still_get_out_from_inside_a_half_typed_chord() {
    let keymap = Keymap::defaults();
    for (escape, expected) in [(Key::Esc, Action::Cancel), (Key::CTRL_C, Action::Quit)] {
        let mut pending = Pending::default();
        feed(&keymap, Mode::Normal, &mut pending, "3g");
        assert!(!pending.is_empty(), "something is half-typed");
        assert!(
            matches!(
                keymap.resolve(Mode::Normal, &mut pending, escape),
                Resolution::Run { action, .. } if action == expected
            ),
            "{escape} did not get out from under a pending chord"
        );
        assert!(pending.is_empty(), "{escape} left something pending");
    }
}

#[test]
fn no_binding_may_start_with_a_reserved_key() {
    // The structural half of "Esc always gets out": if `<esc>j` could be
    // bound, a bare Esc would become merely *pending*, and a mode nobody can
    // leave is the worst failure a modal UI has.
    let mut keymap = Keymap::defaults();
    for text in ["<esc>", "<esc>j", "<c-c>", "<c-c>x"] {
        let error = keymap.bind(Mode::Normal, chord(text), Action::Quit);
        assert!(
            error.is_err(),
            "{text} was accepted as a binding, which can strand the user"
        );
    }
    // Ctrl-anything-else is fair game — only the two escapes are reserved.
    assert!(keymap
        .bind(Mode::Normal, chord("<c-d>"), Action::Quit)
        .is_ok());
}

// ---------------------------------------------------------------------------
// binding rules
// ---------------------------------------------------------------------------

#[test]
fn a_binding_that_would_make_another_unreachable_is_refused() {
    let mut keymap = Keymap::defaults();
    // `gg` is bound in normal mode, so `g` alone would shadow it — and with
    // no timeout to wait out, the shadowed one could never be typed.
    let error = match keymap.bind(Mode::Normal, chord("g"), Action::Quit) {
        Ok(()) => panic!("g was allowed to shadow gg"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("gg"), "{error}");
    assert!(error.contains("unbind"), "the way out is named: {error}");

    // Unbinding the longer one first is exactly what the message says to do.
    assert_eq!(
        keymap.unbind(Mode::Normal, &chord("gg")),
        Some(Action::CursorTop)
    );
    assert!(keymap.bind(Mode::Normal, chord("g"), Action::Quit).is_ok());
}

#[test]
fn a_chord_cannot_be_bound_where_keys_are_text() {
    let mut keymap = Keymap::defaults();
    let error = match keymap.bind(Mode::Insert, chord("jk"), Action::Cancel) {
        Ok(()) => panic!("a chord was accepted in insert mode"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("insert"), "{error}");
    // A single key is fine there.
    assert!(keymap
        .bind(Mode::Insert, chord("<c-w>"), Action::InputBackspace)
        .is_ok());
}

#[test]
fn rebinding_a_chord_replaces_what_it_did() {
    let mut keymap = Keymap::defaults();
    assert!(keymap
        .bind(Mode::Normal, chord("j"), Action::CursorUp)
        .is_ok());
    assert_eq!(
        once(&keymap, Mode::Normal, Key::Char('j')),
        run(Action::CursorUp)
    );
}

// ---------------------------------------------------------------------------
// layers
// ---------------------------------------------------------------------------

#[test]
fn the_viewer_inherits_normal_mode_without_restating_it() {
    let keymap = Keymap::defaults();
    assert!(
        keymap.layer(Mode::Viewer).count() == 0,
        "the viewer's own layer is empty on purpose — it inherits"
    );
    assert_eq!(
        once(&keymap, Mode::Viewer, Key::Char('j')),
        run(Action::CursorDown)
    );
    assert_eq!(
        once(&keymap, Mode::Viewer, Key::Char('q')),
        run(Action::Back)
    );
}

#[test]
fn an_overlay_mode_does_not_inherit_the_list_bindings() {
    // What keeps `j` from scrolling the message list behind a modal:
    // structure, not an early return somebody has to remember to write.
    let keymap = Keymap::defaults();
    assert_eq!(
        once(&keymap, Mode::Confirm, Key::Char('j')),
        Resolution::Unbound(Key::Char('j'))
    );
    assert_eq!(
        once(&keymap, Mode::Confirm, Key::Char('a')),
        Resolution::Unbound(Key::Char('a'))
    );
    // `Mode::Help` used to be the example above. It now binds `j` in its
    // *own* layer — task 103's manual reuses this layer and is a document
    // that scrolls — so the property has to be stated the way it was always
    // meant: nothing arrives here *from* `Mode::Normal`. `a` is Normal's
    // archive key, and reaching it through a modal is the bug.
    assert_eq!(
        once(&keymap, Mode::Help, Key::Char('j')),
        run(Action::CursorDown),
        "the help layer binds its own `j`"
    );
    assert_eq!(
        once(&keymap, Mode::Help, Key::Char('a')),
        Resolution::Unbound(Key::Char('a')),
        "but Normal's archive key does not fall through to it"
    );
    // The picker is a list of its own, so it binds its own movement.
    assert_eq!(
        once(&keymap, Mode::Pick, Key::Char('j')),
        run(Action::CursorDown)
    );
}

#[test]
fn a_nearer_layer_shadows_the_one_it_inherits_from() {
    let keymap = Keymap::defaults();
    assert_eq!(
        once(&keymap, Mode::Normal, Key::Char('o')),
        run(Action::OpenHtml)
    );
    assert_eq!(
        once(&keymap, Mode::Visual, Key::Char('o')),
        run(Action::VisualSwapEnds),
        "visual mode's own `o` wins over the one it inherits"
    );
    // And the help screen lists the winner, not both.
    assert!(keymap.chords_for(Mode::Visual, Action::OpenHtml).is_empty());
    assert_eq!(
        keymap.chords_for(Mode::Visual, Action::VisualSwapEnds),
        vec![chord("o")]
    );
}

#[test]
fn chords_for_reports_every_way_to_press_an_action_once() {
    let keymap = Keymap::defaults();
    assert_eq!(
        keymap.chords_for(Mode::Normal, Action::CursorDown),
        vec![chord("j"), chord("<down>")]
    );
    assert_eq!(
        keymap.chords_for(Mode::Normal, Action::Quit),
        vec![chord("<c-c>")],
        "inherited from the global layer"
    );
}

#[test]
fn modes_name_themselves_the_way_keys_toml_spells_them() {
    for mode in Mode::CONFIGURABLE {
        assert_eq!(Mode::from_id(mode.id()), Some(*mode));
    }
    assert_eq!(
        Mode::from_id("global"),
        None,
        "the global layer is not the user's to rebind"
    );
    assert_eq!(Mode::from_id("nonsense"), None);
    assert!(!Mode::Insert.takes_counts());
    assert!(!Mode::Insert.allows_chords());
    assert!(Mode::Normal.takes_counts() && Mode::Normal.allows_chords());
    // A typing overlay is a text field too: `from:alice2` must not turn its
    // `2` into a repeat count, and a chord there would hold back the next
    // character of the query.
    assert!(!Mode::Prompt.takes_counts());
    assert!(!Mode::Prompt.allows_chords());
    assert!(Mode::Menu.takes_counts() && Mode::Menu.allows_chords());
}
