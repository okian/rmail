//! Task 91's band: what it shows, when it shows nothing, and what it says
//! about a binding no keyboard can deliver.
//!
//! Driven through `tui::model::update` wherever the question is "what does the
//! band do after these keys", so the states under test are ones a user can
//! actually get into rather than ones assembled by hand. The render assertions
//! go through `tui::view` against ratatui's `TestBackend`, for the two claims
//! that are genuinely about drawing: that the struck-through entry is struck
//! through, and that the band takes rows away from nothing else when it is
//! absent.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use rmail_core::keymap::{Action, Chord, Mode};

use super::*;
use crate::tui::model::{update, Account, Cmd, Folder, MessageRow, Msg};
use crate::tui::view;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn loaded() -> Model {
    let mut model = Model::new();
    model.account = Some(Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    });
    model.folders = vec![Folder {
        id: 1,
        name: "INBOX".to_owned(),
        message_count: 3,
    }];
    model.open_folder = Some(1);
    model.messages = (10..13)
        .map(|id| MessageRow {
            id,
            subject: format!("subject {id}"),
            from: "Alice".to_owned(),
            from_addr: Some("alice@example.com".to_owned()),
            date: Some(1_700_000_000 + id),
            flags: Vec::new(),
            has_attachments: false,
        })
        .collect();
    model
}

fn press(model: &mut Model, key: Key) -> Vec<Cmd> {
    update(model, Msg::Key(key))
}

fn keys(model: &mut Model, sequence: &str) {
    for c in sequence.chars() {
        press(model, Key::Char(c));
    }
}

fn chord(text: &str) -> Chord {
    match Chord::parse(text) {
        Ok(chord) => chord,
        Err(error) => panic!("{text:?} should parse: {error}"),
    }
}

fn bind(model: &mut Model, mode: Mode, text: &str, action: Action) {
    match model.keymap.bind(mode, chord(text), action) {
        Ok(()) => {}
        Err(error) => panic!("binding {text:?} in {} failed: {error}", mode.id()),
    }
}

fn shown(model: &Model) -> Band {
    match band(model) {
        Some(band) => band,
        None => panic!("expected a band, and there is none"),
    }
}

/// The keys the band offers, in order.
fn offered(band: &Band) -> Vec<String> {
    band.entries.iter().map(|e| e.keys.clone()).collect()
}

fn of_kind(band: &Band, kind: Kind) -> Vec<String> {
    band.entries
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| e.keys.clone())
        .collect()
}

/// Parses the `N` out of a rendered row's `+N`, if the row has one.
fn overflow_count(row: &str) -> Option<u32> {
    let after = row.rsplit_once('+')?.1;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Render `model` and flatten the buffer into one string per row.
fn draw(model: &Model, width: u16, height: u16) -> Vec<String> {
    let mut terminal = match Terminal::new(TestBackend::new(width, height)) {
        Ok(terminal) => terminal,
        Err(error) => panic!("the test backend would not start: {error}"),
    };
    if let Err(error) = terminal.draw(|f| view::render(model, f)) {
        panic!("rendering failed: {error}");
    }
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

/// Whether any cell on screen carries `modifier`, over a cell whose symbol is
/// in `within`.
fn styled_cells(model: &Model, modifier: ratatui::style::Modifier) -> Vec<String> {
    let mut terminal = match Terminal::new(TestBackend::new(120, 30)) {
        Ok(terminal) => terminal,
        Err(error) => panic!("the test backend would not start: {error}"),
    };
    if let Err(error) = terminal.draw(|f| view::render(model, f)) {
        panic!("rendering failed: {error}");
    }
    let buffer = terminal.backend().buffer().clone();
    let mut found = Vec::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if cell.modifier.contains(modifier) {
                found.push(cell.symbol().to_owned());
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// when it draws, and when it does not
// ---------------------------------------------------------------------------

#[test]
fn nothing_pending_draws_no_band() {
    let model = loaded();
    assert!(band(&model).is_none());
}

#[test]
fn a_count_only_pending_draws_no_band() {
    // `3` alone is a repeat waiting for a command, and every binding in the
    // mode is still available — a band listing all of them would say only
    // "the keyboard works".
    let mut model = loaded();
    keys(&mut model, "3");
    assert_eq!(model.pending.count(), Some(3), "the count is pending");
    assert!(model.pending.keys().is_empty());
    assert!(band(&model).is_none());
}

#[test]
fn a_pending_chord_draws_a_band_immediately() {
    let mut model = loaded();
    keys(&mut model, "g");
    let band = shown(&model);
    assert_eq!(band.title, "g");
    assert!(
        offered(&band).contains(&"g".to_owned()),
        "the only continuation of `g` is `gg`: {band:?}"
    );
    assert!(band.warning.is_none());
}

#[test]
fn a_count_then_a_chord_draws_the_band_with_the_count_in_the_title() {
    let mut model = loaded();
    keys(&mut model, "3g");
    let band = shown(&model);
    assert_eq!(
        band.title, "g",
        "the title is what is half-typed towards a binding; the count is the \
         status line's business and is shown there"
    );
    assert!(offered(&band).contains(&"g".to_owned()));
}

#[test]
fn resolving_the_chord_takes_the_band_away_again() {
    let mut model = loaded();
    keys(&mut model, "g");
    assert!(band(&model).is_some());
    keys(&mut model, "g");
    assert!(
        band(&model).is_none(),
        "`gg` resolved, so nothing is half-typed"
    );
}

#[test]
fn the_command_overlay_draws_candidates_instead() {
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    keys(&mut model, "message.");
    let band = shown(&model);
    assert_eq!(band.title, ":message.");
    let offered = offered(&band);
    for expected in ["archive", "delete", "reply"] {
        assert!(
            offered.contains(&expected.to_owned()),
            "{expected} is a child of `message`: {offered:?}"
        );
    }
    assert!(
        band.warning.is_none(),
        "the verb registry is one namespace with no layers, so nothing in it \
         can shadow anything else"
    );
    assert_eq!(
        of_kind(&band, Kind::Pinned),
        ["<esc>", "<c-c>"],
        "the way out is in every band, the command line's included — the \
         overlay's own hint row saying so as well does not make this the same \
         row"
    );
}

#[test]
fn a_command_candidate_with_children_reads_as_a_group() {
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    let band = shown(&model);
    // `message` has children (`message.archive`, …); `quit` does not — and,
    // unlike this test's original example, is a poor candidate for ever
    // growing one. (`help` was that original example; task 102 gave it a
    // child of its own — `help.rebind` — which correctly flips it to
    // `Group` here too, the same way `search`/`search.explain` already
    // coexist as a bare leaf with a dotted sibling.)
    assert!(
        of_kind(&band, Kind::Group).contains(&"message".to_owned()),
        "{band:?}"
    );
    assert!(
        of_kind(&band, Kind::Run).contains(&"quit".to_owned()),
        "{band:?}"
    );
}

// ---------------------------------------------------------------------------
// what it says
// ---------------------------------------------------------------------------

/// Every key in any chord of any layer, plus one nothing binds.
///
/// The universe the claim below is quantified over: a band offering a key
/// nothing extends and a band missing one that something does are both
/// failures, and only a universe wider than the answer can catch the second.
fn every_key(keymap: &Keymap) -> Vec<Key> {
    let mut seen: std::collections::BTreeSet<Key> = std::collections::BTreeSet::new();
    for mode in Mode::CONFIGURABLE {
        for layer in mode.chain() {
            for (chord, _) in keymap.layer(*layer) {
                seen.extend(chord.keys().iter().copied());
            }
        }
    }
    seen.insert(Key::Char('~'));
    seen.into_iter().collect()
}

/// Whether `prefix + key` is a sequence the engine would hold or complete,
/// asked of `resolve` and `lookup` rather than of `continuations`.
///
/// The independent half of the assertion below. `resolve` alone is not enough:
/// a dead sequence retries its own tail, so `g` then `j` *runs* `cursor.down`
/// while `gj` extends nothing — which is why a `Run` is only counted when
/// `lookup` agrees the whole sequence is bound.
fn extends(keymap: &Keymap, mode: Mode, prefix: &[Key], key: Key) -> bool {
    let mut pending = rmail_core::keymap::Pending::default();
    let mut last = None;
    for pressed in prefix.iter().copied().chain(std::iter::once(key)) {
        last = Some(keymap.resolve(mode, &mut pending, pressed));
    }
    let whole: Vec<Key> = prefix.iter().copied().chain(std::iter::once(key)).collect();
    if pending.keys() == whole.as_slice() {
        return true;
    }
    matches!(last, Some(rmail_core::keymap::Resolution::Run { .. }))
        && Chord::new(whole).is_ok_and(|chord| keymap.lookup(mode, &chord).is_some())
}

/// The claim the band rests on: it offers exactly the keys that could extend
/// what is pending — no key that does nothing, and none missing.
///
/// Quantified over every mode and every prefix the engine actually holds, and
/// checked against [`extends`], which is built from `resolve` and `lookup`
/// rather than from `Keymap::continuations` — otherwise this would be the
/// derivation compared with itself, which is the shape of a test that cannot
/// fail.
#[test]
fn the_bands_key_set_is_the_extendable_key_set_for_every_mode_and_prefix() {
    let keymap = Keymap::defaults();
    let universe = every_key(&keymap);
    let mut checked = 0;
    for mode in Mode::CONFIGURABLE {
        let mut chords = Vec::new();
        for layer in mode.chain() {
            for (chord, _) in keymap.layer(*layer) {
                chords.push(chord.clone());
            }
        }
        for chord in chords {
            for len in 1..chord.keys().len() {
                let prefix: Vec<Key> = chord.keys().iter().copied().take(len).collect();
                let mut pending = rmail_core::keymap::Pending::default();
                for key in &prefix {
                    keymap.resolve(*mode, &mut pending, *key);
                }
                if pending.keys() != prefix.as_slice() {
                    continue;
                }
                // `chord_band` rather than `band`, because `band` reads the
                // mode off the *model* and this loop is quantifying over
                // modes. Which mode `band` asks about is
                // `the_band_reads_the_mode_the_model_is_in`'s claim, not this
                // one — a first draft conflated the two and compared Normal's
                // band against Help's answer.
                let band = chord_band(&keymap, *mode, &prefix);
                let offered: std::collections::BTreeSet<String> = band
                    .entries
                    .iter()
                    .filter(|e| matches!(e.kind, Kind::Run | Kind::Group))
                    .map(|e| e.keys.clone())
                    .collect();
                let expected: std::collections::BTreeSet<String> = universe
                    .iter()
                    .filter(|key| extends(&keymap, *mode, &prefix, **key))
                    .map(ToString::to_string)
                    .collect();
                assert_eq!(
                    offered,
                    expected,
                    "in {} after {prefix:?}: the band's keys and the keys the \
                     engine would actually accept have to be the same set",
                    mode.id()
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 0,
        "the default map has prefixes, so this proved something"
    );
}

#[test]
fn the_band_reads_the_mode_the_model_is_in() {
    // `gg` is bound in Normal and in Menu, and the manual's layer (`Mode::Help`)
    // adds `g/` next to it — so the same pending `g` has a different answer
    // depending on which layer the keyboard is reading, and the band has to ask
    // the model rather than assume.
    let mut normal = loaded();
    keys(&mut normal, "g");
    assert_eq!(normal.mode(), Mode::Normal);
    assert_eq!(offered(&shown(&normal)), ["g", "<esc>", "<c-c>"]);

    let mut help = loaded();
    press(&mut help, Key::Char('?'));
    assert_eq!(help.mode(), Mode::Help);
    keys(&mut help, "g");
    let offered = offered(&shown(&help));
    assert!(
        offered.contains(&"/".to_owned()),
        "`g/` is `manual.grep`, bound in the layer the manual and `?` share: \
         {offered:?}"
    );
}

#[test]
fn the_band_offers_no_key_the_engine_would_refuse() {
    // The same claim from the other side, on a keymap the defaults do not
    // produce: a prefix with several continuations, one key that is not one of
    // them, and a band that must not mention it.
    let mut model = loaded();
    bind(&mut model, Mode::Normal, "za", Action::AiPanel);
    bind(&mut model, Mode::Normal, "zq", Action::AiQuick);
    keys(&mut model, "z");
    let band = shown(&model);
    let offered = offered(&band);
    assert!(offered.contains(&"a".to_owned()), "{offered:?}");
    assert!(offered.contains(&"q".to_owned()), "{offered:?}");
    assert!(
        !offered.contains(&"j".to_owned()),
        "`zj` is not a binding, and `j` after `z` runs `cursor.down` off the \
         retried tail rather than completing anything: {offered:?}"
    );
}

#[test]
fn a_group_is_labelled_by_the_common_prefix_of_its_members() {
    let mut model = loaded();
    bind(&mut model, Mode::Normal, "za", Action::AiPanel);
    bind(&mut model, Mode::Normal, "zq", Action::AiQuick);
    keys(&mut model, "z");
    let band = shown(&model);
    let z = band
        .entries
        .iter()
        .find(|e| e.keys == "a")
        .cloned()
        .unwrap_or_else(|| panic!("expected an entry for `a`: {band:?}"));
    assert_eq!(z.label, "ai.panel", "a leaf is labelled by what it runs");

    // And one level up, `z` itself is the group.
    let mut model = loaded();
    bind(&mut model, Mode::Normal, "za", Action::AiPanel);
    bind(&mut model, Mode::Normal, "zq", Action::AiQuick);
    model.pending.clear();
    let band = chord_band(&model.keymap, Mode::Normal, &[]);
    let group = band
        .entries
        .iter()
        .find(|e| e.keys == "z")
        .cloned()
        .unwrap_or_else(|| panic!("expected an entry for `z`: {band:?}"));
    assert_eq!(
        group.label, "ai…",
        "derived from the member ids, never a hand-written group name"
    );
    assert_eq!(group.kind, Kind::Group);
}

#[test]
fn a_group_with_nothing_in_common_is_labelled_by_its_size() {
    let mut model = loaded();
    bind(&mut model, Mode::Normal, "zh", Action::Help);
    bind(&mut model, Mode::Normal, "zs", Action::SearchOpen);
    let band = chord_band(&model.keymap, Mode::Normal, &[]);
    let group = band
        .entries
        .iter()
        .find(|e| e.keys == "z")
        .cloned()
        .unwrap_or_else(|| panic!("expected an entry for `z`: {band:?}"));
    assert_eq!(group.label, "2 commands");
}

#[test]
fn the_ways_out_are_pinned_in_every_band() {
    let mut model = loaded();
    keys(&mut model, "g");
    let band = shown(&model);
    assert_eq!(
        of_kind(&band, Kind::Pinned),
        ["<esc>", "<c-c>"],
        "the way out is in every band, whatever else is: {band:?}"
    );
    let labels: Vec<String> = band
        .entries
        .iter()
        .filter(|e| e.kind == Kind::Pinned)
        .map(|e| e.label.clone())
        .collect();
    assert_eq!(
        labels,
        ["cancel", "quit"],
        "labelled by whatever they are bound to rather than by a literal here"
    );
}

#[test]
fn the_pinned_entries_survive_the_entry_cap() {
    // A `keys.toml` with more continuations than the band carries must not
    // push the way out off the end of it — that is the whole point of pinning.
    let mut model = loaded();
    for c in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP".chars() {
        bind(&mut model, Mode::Normal, &format!("z{c}"), Action::Help);
    }
    keys(&mut model, "z");
    let band = shown(&model);
    assert_eq!(band.entries.len(), MAX_ENTRIES);
    assert_eq!(of_kind(&band, Kind::Pinned), ["<esc>", "<c-c>"]);
    assert!(band.dropped > 0, "and it says how many it dropped");
}

// ---------------------------------------------------------------------------
// a binding no keyboard can deliver
// ---------------------------------------------------------------------------

#[test]
fn a_binding_killed_by_cross_layer_shadowing_is_listed_with_a_warning() {
    // `Normal` has the three-key `zab`; `Visual` binds the two-key `za`. In
    // Visual, `a` after `z` runs Visual's binding and nothing ever waits for
    // `b`, so `zab` is a binding the keyboard cannot deliver. `Keymap::bind`
    // cannot refuse this — it sees one layer — so the band is where somebody
    // finds out.
    let mut model = loaded();
    bind(&mut model, Mode::Normal, "zab", Action::AiPanel);
    bind(&mut model, Mode::Visual, "za", Action::AiQuick);
    press(&mut model, Key::Char('v'));
    assert_eq!(model.mode(), Mode::Visual);
    keys(&mut model, "z");

    let band = shown(&model);
    assert_eq!(
        of_kind(&band, Kind::Dead),
        ["zab"],
        "the dead binding is named, not merely counted: {band:?}"
    );
    let dead = band
        .entries
        .iter()
        .find(|e| e.kind == Kind::Dead)
        .cloned()
        .unwrap_or_else(|| panic!("expected a dead entry: {band:?}"));
    assert_eq!(
        dead.label, "ai.panel",
        "labelled with what it would have done — `lookup` on a dead chord \
         answers the killer's action, so this is the only place its own \
         meaning survives"
    );
    let warning = band.warning.clone().unwrap_or_default();
    assert!(warning.contains("cannot be typed"), "{warning}");

    // The engine agrees: `a` runs Visual's binding and leaves nothing pending.
    let cmds = press(&mut model, Key::Char('a'));
    assert!(cmds.is_empty(), "ai.quick opens a panel and issues nothing");
    assert!(model.pending.keys().is_empty(), "nothing waits for `b`");
}

#[test]
fn a_healthy_band_carries_no_dead_entries_and_no_warning() {
    let mut model = loaded();
    keys(&mut model, "g");
    let band = shown(&model);
    assert!(of_kind(&band, Kind::Dead).is_empty(), "{band:?}");
    assert!(band.warning.is_none());
    assert_eq!(
        model.keymap.shadowed_across_layers(),
        Vec::new(),
        "and the default bindings shadow nothing, which is why"
    );
}

// ---------------------------------------------------------------------------
// drawing it
// ---------------------------------------------------------------------------

#[test]
fn the_band_draws_its_entries_above_the_status_line() {
    let mut model = loaded();
    keys(&mut model, "g");
    let rendered = draw(&model, 120, 24);
    let band_row = rendered
        .get(rendered.len().saturating_sub(2))
        .cloned()
        .unwrap_or_default();
    assert!(band_row.contains("cursor.top"), "{band_row:?}");
    assert!(band_row.contains("<esc>"), "{band_row:?}");
    let status = rendered.last().cloned().unwrap_or_default();
    assert!(
        !status.contains("cursor.top"),
        "the band has a row of its own: {status:?}"
    );
}

#[test]
fn no_band_means_no_row_taken_from_anything_else() {
    let model = loaded();
    let without = draw(&model, 120, 24);
    let mut with = loaded();
    keys(&mut with, "g");
    let with = draw(&with, 120, 24);
    assert_eq!(without.len(), with.len(), "same terminal, same rows");
    assert_ne!(
        without.get(without.len().saturating_sub(2)),
        with.get(with.len().saturating_sub(2)),
        "and the band is drawn over what was there rather than beside it"
    );
}

#[test]
fn a_dead_entry_is_drawn_struck_through() {
    let mut model = loaded();
    bind(&mut model, Mode::Normal, "zab", Action::AiPanel);
    bind(&mut model, Mode::Visual, "za", Action::AiQuick);
    press(&mut model, Key::Char('v'));
    keys(&mut model, "z");
    let struck: String = styled_cells(&model, ratatui::style::Modifier::CROSSED_OUT)
        .into_iter()
        .collect();
    assert!(
        struck.contains("zab"),
        "a binding the keyboard cannot deliver has to look different from one \
         that is merely uninteresting: {struck:?}"
    );
    assert!(
        !struck.contains("cursor"),
        "and nothing else is struck through: {struck:?}"
    );
}

#[test]
fn the_warning_gets_a_row_of_its_own_rather_than_reflowing_the_entries() {
    let mut model = loaded();
    bind(&mut model, Mode::Normal, "zab", Action::AiPanel);
    bind(&mut model, Mode::Visual, "za", Action::AiQuick);
    press(&mut model, Key::Char('v'));
    keys(&mut model, "z");
    let rendered = draw(&model, 120, 24);
    let entries = rendered
        .get(rendered.len().saturating_sub(3))
        .cloned()
        .unwrap_or_default();
    let warning = rendered
        .get(rendered.len().saturating_sub(2))
        .cloned()
        .unwrap_or_default();
    assert!(entries.contains("zab"), "{entries:?}");
    assert!(warning.contains("cannot be typed"), "{warning:?}");
}

#[test]
fn the_band_survives_a_terminal_too_narrow_to_hold_it() {
    let mut model = loaded();
    keys(&mut model, "g");
    // No assertion beyond "this returns": every overlay here is expected to
    // clamp rather than to be handed a terminal that fits.
    assert_eq!(draw(&model, 10, 6).len(), 6);
}

#[test]
fn a_binding_can_be_killed_by_a_farther_layer_too() {
    // The mirror of `a_dead_entry_is_drawn_struck_through`: there, the
    // shorter chord ("za") happens to be in the *nearer* layer (`Visual`)
    // relative to the longer one ("zab", `Normal`) — which is also the
    // arrangement that would make a wrong "a nearer layer runs first"
    // warning read as correct. Reversed here (shorter in `Normal`, the
    // farther layer; longer in `Visual`, the nearer one) to prove the
    // warning names no particular layer, because `resolve` does not care
    // which layer is nearer — only which chord is shorter.
    let mut model = loaded();
    bind(&mut model, Mode::Normal, "za", Action::AiQuick);
    bind(&mut model, Mode::Visual, "zab", Action::AiPanel);
    press(&mut model, Key::Char('v'));
    keys(&mut model, "z");

    let struck: String = styled_cells(&model, ratatui::style::Modifier::CROSSED_OUT)
        .into_iter()
        .collect();
    assert!(struck.contains("zab"), "{struck:?}");

    let rendered = draw(&model, 120, 24).join("\n");
    assert!(
        rendered.contains("cannot be typed"),
        "the warning still has to say what happened, not just avoid saying \
         the wrong thing: {rendered}"
    );
    assert!(
        !rendered.contains("nearer"),
        "the warning must not claim a specific layer's nearness: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// the pinned ways out survive an overflowing band
// ---------------------------------------------------------------------------

#[test]
fn the_pinned_ways_out_are_visible_when_the_command_band_overflows_the_terminal() {
    // Regression: every top-level verb rendered on one unwrapped line ran to
    // 244 columns on the real registry — `<esc>`/`<c-c>` landed roughly 96
    // columns off the right edge of a 120-column terminal, the width every
    // other test in this file already uses.
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    let band = shown(&model);
    assert!(
        band.entries.len() > 20,
        "the repro needs the overflow to actually happen: {band:?}"
    );

    let rendered = draw(&model, 120, 24).join("\n");
    assert!(rendered.contains("<esc>"), "{rendered}");
    assert!(rendered.contains("<c-c>"), "{rendered}");
}

#[test]
fn the_pinned_ways_out_survive_a_busy_chord_band_too() {
    // Not exercised by the shipped keymap today — task 105's `<space>`
    // leader, which depends on this task, is what will make a chord prefix
    // with a dozen live continuations real. Simulated directly so the fix
    // does not wait on that task to land before it is provable.
    let mut model = loaded();
    for (i, c) in ('a'..='l').enumerate() {
        let action = if i % 2 == 0 {
            Action::AiPanel
        } else {
            Action::AiQuick
        };
        bind(&mut model, Mode::Normal, &format!("z{c}"), action);
    }
    keys(&mut model, "z");
    let band = shown(&model);
    assert!(
        band.entries.len() > 8,
        "the setup needs enough entries to actually overflow: {band:?}"
    );

    let rendered = draw(&model, 120, 24).join("\n");
    assert!(rendered.contains("<esc>"), "{rendered}");
    assert!(rendered.contains("<c-c>"), "{rendered}");
}

#[test]
fn a_band_that_fits_shows_no_overflow_indicator() {
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    keys(&mut model, "message a");
    let band = shown(&model);
    assert!(
        band.entries.len() < 6,
        "the setup needs a narrow enough match set: {band:?}"
    );

    let rendered = draw(&model, 120, 24);
    let band_row = rendered
        .get(rendered.len().saturating_sub(2))
        .cloned()
        .unwrap_or_default();
    assert!(
        overflow_count(&band_row).is_none(),
        "nothing was cut, so no +N belongs on the band's own row: {band_row:?}"
    );
}

#[test]
fn a_dropped_count_is_reported_at_every_width_not_just_narrow_ones() {
    // `band.dropped` (from `finish`'s `MAX_ENTRIES` cap) is settled before a
    // terminal width exists at all, so once it is nonzero a `+N` is owed at
    // every width — including one comfortably wide enough that every live
    // entry, taken alone, would have fit with room to spare. A reservation
    // that only ever looks at the live entries' own total width can walk
    // right past that: it would draw all of them thinking nothing needed
    // cutting, then try to append a suffix nobody left room for.
    let mut model = loaded();
    for c in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOP".chars() {
        bind(&mut model, Mode::Normal, &format!("z{c}"), Action::Help);
    }
    keys(&mut model, "z");
    let band = shown(&model);
    assert!(
        band.dropped > 0,
        "the cap alone must already be biting: {band:?}"
    );
    let known_dropped = u32::try_from(band.dropped).unwrap_or(u32::MAX);

    // The sweep starts well past the pinned column's own width (measured at
    // 26 for the default `<esc>`/`<c-c>` labels), not at some narrow width:
    // below that, `Constraint::Min(0)` on the entry+suffix side has already
    // given up the whole row to `Constraint::Length` on the pinned side —
    // deliberately, per `render_band`'s own doc comment — so there is no
    // column left for a `+N` to appear in at all, regardless of what this
    // function's arithmetic computes. That narrower regime is covered by
    // `the_band_survives_a_terminal_too_narrow_to_hold_it` instead.
    for width in (60..=400).step_by(2) {
        let rendered = draw(&model, width, 24);
        let band_row = rendered
            .get(rendered.len().saturating_sub(2))
            .cloned()
            .unwrap_or_default();
        match overflow_count(&band_row) {
            Some(reported) => assert!(
                reported >= known_dropped,
                "width {width}: at least the {known_dropped} the cap already \
                 dropped must be reported, not swallowed by a reservation \
                 that only looked at the live entries: {band_row:?}"
            ),
            None => panic!(
                "width {width}: the cap alone already owes a +N and none is \
                 on screen: {band_row:?}"
            ),
        }
    }

    // Wide enough that the terminal itself cuts nothing: the count reported
    // must be exactly what the cap dropped, no more.
    let rendered = draw(&model, 600, 24);
    let band_row = rendered
        .get(rendered.len().saturating_sub(2))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        overflow_count(&band_row),
        Some(known_dropped),
        "at 600 columns only the cap should be reporting, not the terminal \
         clip too: {band_row:?}"
    );
}
