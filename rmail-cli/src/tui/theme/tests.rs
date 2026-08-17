use ratatui::style::{Color, Modifier};

use super::*;

/// A field's name and accessor.
type Field = (&'static str, fn(&Theme) -> Style);

/// Every field of [`Theme`], paired with its name — the single place that
/// enumerates them, so a test that means "every field" (mono's color-free
/// claim, the dark/light pin) cannot silently skip one a future field adds.
const FIELDS: &[Field] = &[
    ("border_focus", |t| t.border_focus),
    ("border_blur", |t| t.border_blur),
    ("sel_focus", |t| t.sel_focus),
    ("sel_blur", |t| t.sel_blur),
    ("sel_row", |t| t.sel_row),
    ("toast", |t| t.toast),
    ("muted", |t| t.muted),
    ("emphasis", |t| t.emphasis),
    ("accent", |t| t.accent),
    ("match_hl", |t| t.match_hl),
    ("ok", |t| t.ok),
    ("err", |t| t.err),
    ("warn", |t| t.warn),
    ("mode_indicator", |t| t.mode_indicator),
    ("unread", |t| t.unread),
    ("flagged", |t| t.flagged),
    ("attachment", |t| t.attachment),
    ("finder_kind", |t| t.finder_kind),
];

#[test]
fn every_built_in_is_a_distinct_theme() {
    let built_ins = [
        Theme::dark(),
        Theme::light(),
        Theme::mono(),
        Theme::high_contrast(),
    ];
    for (i, a) in built_ins.iter().enumerate() {
        for (j, b) in built_ins.iter().enumerate() {
            assert!(i == j || a != b, "themes at {i} and {j} are identical");
        }
    }
}

#[test]
fn default_is_dark() {
    assert_eq!(Theme::default(), Theme::dark());
    assert_eq!(ThemeName::default(), ThemeName::Dark);
}

/// `Theme::mono`'s whole reason to exist: no field may carry meaning by
/// color, because a `mono` terminal cannot render one. Every field is
/// checked, not a sample — this is what makes the claim in `Theme::mono`'s
/// doc comment true rather than aspirational.
#[test]
fn mono_sets_no_foreground_or_background_anywhere() {
    let mono = Theme::mono();
    for (name, get) in FIELDS {
        let style = get(&mono);
        assert!(style.fg.is_none(), "mono's {name} sets a foreground color");
        assert!(style.bg.is_none(), "mono's {name} sets a background color");
    }
}

/// The flip side of the above: a field that carries no *color* had better
/// carry a *modifier*, or `mono` really would be indistinguishable chrome.
/// Five deliberate exceptions, each an "unmarked baseline" rather than a
/// gap: `ok` (success needs no marker, only failure/warning does — the same
/// asymmetry an unread message has over a read one), `border_blur` (its
/// pairing with bold `border_focus` *is* the distinction — a plain border
/// next to a bold one still reads as "not this one"), and `unread`/
/// `flagged`/`attachment` (the glyph itself — `●`/`★`/`@`, present or a
/// blank space — already carries the meaning these three exist for; styling
/// the glyph on top would be redundant, not clearer).
const NO_MODIFIER_NEEDED: &[&str] = &["ok", "border_blur", "unread", "flagged", "attachment"];

#[test]
fn mono_distinguishes_every_field_by_a_modifier_unless_its_baseline_is_unmarked() {
    let mono = Theme::mono();
    for (name, get) in FIELDS {
        let style = get(&mono);
        if NO_MODIFIER_NEEDED.contains(name) {
            continue;
        }
        assert!(
            !style.add_modifier.is_empty(),
            "mono's {name} carries no color and no modifier — it would be invisible"
        );
    }
}

/// Pins `Theme::dark` to the exact values `view.rs` hardcoded before this
/// module existed — every field, every modifier, via `assert_eq!` on the
/// whole struct rather than a field-by-field spot check, so a value this
/// test does not happen to mention (an added `Modifier::BOLD` on `toast`,
/// say) cannot slip through green. `historical()` is built from the same
/// literals `git show`'s pre-refactor `view.rs` used, independent of
/// `Theme::dark`'s own construction, so this cannot pass by both sides
/// drifting the same way. This is the theme-level half of the
/// byte-identical-frame claim; `tui::view::tests` proves the render half by
/// drawing real frames.
#[test]
fn dark_is_the_historical_styling() {
    fn historical() -> Theme {
        Theme {
            border_focus: Style::new().fg(Color::Cyan),
            border_blur: Style::new().fg(Color::DarkGray),
            sel_focus: Style::new()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            sel_blur: Style::new().add_modifier(Modifier::REVERSED),
            sel_row: Style::new().bg(Color::DarkGray).fg(Color::White),
            toast: Style::new()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            muted: Style::new().fg(Color::DarkGray),
            emphasis: Style::new().add_modifier(Modifier::BOLD),
            accent: Style::new().fg(Color::Cyan),
            match_hl: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ok: Style::new().fg(Color::Green),
            err: Style::new().fg(Color::Red),
            warn: Style::new().fg(Color::Yellow),
            mode_indicator: Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            unread: Style::new().fg(Color::Yellow),
            flagged: Style::new().fg(Color::Yellow),
            attachment: Style::new().fg(Color::Yellow),
            finder_kind: Style::new().fg(Color::Magenta),
        }
    }
    assert_eq!(Theme::dark(), historical());
}

/// `light` has no historical values to pin — it is new in this task — but it
/// is exactly as easy to let its real values drift from what its own doc
/// comment claims them to be, so this checks the same way `dark`'s pin does:
/// against a struct literal built independently of `Theme::light`'s own
/// construction.
#[test]
fn light_matches_what_its_doc_comment_claims() {
    fn claimed() -> Theme {
        Theme {
            border_focus: Style::new().fg(Color::Blue),
            border_blur: Style::new().fg(Color::DarkGray),
            sel_focus: Style::new()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            sel_blur: Style::new().add_modifier(Modifier::REVERSED),
            sel_row: Style::new().bg(Color::Gray).fg(Color::Black),
            toast: Style::new()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            muted: Style::new().fg(Color::DarkGray),
            emphasis: Style::new().add_modifier(Modifier::BOLD),
            accent: Style::new().fg(Color::Blue),
            match_hl: Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
            ok: Style::new().fg(Color::Green),
            err: Style::new().fg(Color::Red),
            warn: Style::new().fg(Color::Magenta),
            mode_indicator: Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            unread: Style::new().fg(Color::Blue),
            flagged: Style::new().fg(Color::Blue),
            attachment: Style::new().fg(Color::Blue),
            finder_kind: Style::new().fg(Color::Magenta),
        }
    }
    assert_eq!(Theme::light(), claimed());
}

/// A `muted` span (a date, a hint, a description) can render inside a
/// visually-selected row (`sel_row`) or under the list cursor (`sel_focus`)
/// — `render_messages` always lets a span's own foreground win over the
/// row's, so if `muted`'s foreground is a *concrete color* equal to one of
/// those backgrounds, that span becomes invisible on exactly the row a user
/// is looking at. `mono` is not a counterexample: both sides are `None`
/// there (mono never calls `.fg()`/`.bg()` at all), which is two unset
/// fields agreeing on "inherit the terminal default," not a color painted
/// over an identical one — so this only compares when both sides are
/// `Some`. `dark` is excluded on purpose: it has this exact `Some`/`Some`
/// collision today (`sel_row.bg == muted.fg == Some(DarkGray)`), inherited
/// unchanged from the pre-refactor code this task's whole mandate is to
/// reproduce byte for byte — not introduced here, and not this task's
/// mandate to fix.
#[test]
fn muted_text_stays_legible_on_a_selected_row_in_every_theme_but_dark() {
    for (name, theme) in [
        ("light", Theme::light()),
        ("mono", Theme::mono()),
        ("high_contrast", Theme::high_contrast()),
    ] {
        if let (Some(muted_fg), Some(sel_row_bg)) = (theme.muted.fg, theme.sel_row.bg) {
            assert_ne!(
                muted_fg, sel_row_bg,
                "{name}: `muted`'s foreground equals `sel_row`'s background"
            );
        }
        if let (Some(muted_fg), Some(sel_focus_bg)) = (theme.muted.fg, theme.sel_focus.bg) {
            assert_ne!(
                muted_fg, sel_focus_bg,
                "{name}: `muted`'s foreground equals `sel_focus`'s background"
            );
        }
    }
}

/// `emphasis` means "stands out by weight," in every theme by definition —
/// unlike the semantic/mail tokens, whose color is free to vary, this one
/// would stop meaning what its name says if some theme dropped the bold.
#[test]
fn emphasis_is_always_bold() {
    for theme in [
        Theme::dark(),
        Theme::light(),
        Theme::mono(),
        Theme::high_contrast(),
    ] {
        assert!(theme.emphasis.add_modifier.contains(Modifier::BOLD));
    }
}

#[test]
fn theme_names_round_trip_through_their_id() {
    for name in ThemeName::ALL {
        assert_eq!(ThemeName::from_id(name.id()), Some(*name));
    }
}

#[test]
fn an_unrecognized_theme_name_resolves_to_nothing() {
    assert_eq!(ThemeName::from_id("solarized"), None);
    assert_eq!(ThemeName::from_id(""), None);
    assert_eq!(ThemeName::from_id("Dark"), None); // case-sensitive, like `Mode::from_id`
}

#[test]
fn every_theme_name_resolves_to_its_constructor() {
    assert_eq!(ThemeName::Dark.resolve(), Theme::dark());
    assert_eq!(ThemeName::Light.resolve(), Theme::light());
    assert_eq!(ThemeName::Mono.resolve(), Theme::mono());
    assert_eq!(ThemeName::HighContrast.resolve(), Theme::high_contrast());
}
