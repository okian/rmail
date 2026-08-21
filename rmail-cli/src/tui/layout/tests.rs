use ratatui::layout::Rect;

use super::*;

fn ctx(focus: Card) -> DeckContext {
    DeckContext {
        focus,
        zoom: None,
        sidebar_visible: true,
        rail_visible: true,
        reader_open: true,
        height_tier: HeightTier::Full,
    }
}

// ---------------------------------------------------------------------
// Breakpoint / height-tier classification
// ---------------------------------------------------------------------

#[test]
fn breakpoint_boundaries_match_the_table() {
    assert_eq!(breakpoint(0), Breakpoint::Xs);
    assert_eq!(breakpoint(79), Breakpoint::Xs);
    assert_eq!(breakpoint(80), Breakpoint::S);
    assert_eq!(breakpoint(119), Breakpoint::S);
    assert_eq!(breakpoint(120), Breakpoint::M);
    assert_eq!(breakpoint(159), Breakpoint::M);
    assert_eq!(breakpoint(160), Breakpoint::L);
    assert_eq!(breakpoint(199), Breakpoint::L);
    assert_eq!(breakpoint(200), Breakpoint::Xl);
    assert_eq!(breakpoint(4000), Breakpoint::Xl);
}

#[test]
fn height_tier_boundaries_match_the_table() {
    assert_eq!(height_tier(0), HeightTier::Bare);
    assert_eq!(height_tier(14), HeightTier::Bare);
    assert_eq!(height_tier(15), HeightTier::Minimal);
    assert_eq!(height_tier(19), HeightTier::Minimal);
    assert_eq!(height_tier(20), HeightTier::Compact);
    assert_eq!(height_tier(24), HeightTier::Compact);
    assert_eq!(height_tier(25), HeightTier::Reduced);
    assert_eq!(height_tier(39), HeightTier::Reduced);
    assert_eq!(height_tier(40), HeightTier::Full);
    assert_eq!(height_tier(500), HeightTier::Full);
}

#[test]
fn height_tier_drop_order_is_exactly_the_table() {
    // >= 40: everything shown, buckets eligible, full gauges.
    assert!(HeightTier::Full.shows_header());
    assert!(!HeightTier::Full.header_gauges_glyph_only());
    assert!(HeightTier::Full.shows_lens_strip());
    assert!(HeightTier::Full.shows_keybar());
    assert!(HeightTier::Full.time_buckets_eligible());
    assert_eq!(HeightTier::Full.whichkey_row_cap(), 2);

    // 25-39: gauges prune to glyph-only; everything else still shown.
    assert!(HeightTier::Reduced.shows_header());
    assert!(HeightTier::Reduced.header_gauges_glyph_only());
    assert!(HeightTier::Reduced.shows_lens_strip());
    assert!(HeightTier::Reduced.shows_keybar());
    assert!(HeightTier::Reduced.time_buckets_eligible());

    // 20-24: lens strip folds into the list title; keybar dropped; which-key
    // capped at 1.
    assert!(HeightTier::Compact.shows_header());
    assert!(!HeightTier::Compact.shows_lens_strip());
    assert!(HeightTier::Compact.lens_strip_folds_into_list_title());
    assert!(!HeightTier::Compact.shows_keybar());
    assert!(!HeightTier::Compact.time_buckets_eligible());
    assert_eq!(HeightTier::Compact.whichkey_row_cap(), 1);

    // 15-19: header dropped; S-stacking becomes slide-between.
    assert!(!HeightTier::Minimal.shows_header());
    assert!(!HeightTier::Minimal.shows_lens_strip());
    assert!(!HeightTier::Minimal.shows_keybar());
    assert!(HeightTier::Minimal.s_stack_becomes_slide_between());

    // < 15: same drops as Minimal, plus every breakpoint (not just S)
    // collapses to a single card — see the `bare_*` tests below.
    assert!(!HeightTier::Bare.shows_header());
    assert!(!HeightTier::Bare.shows_lens_strip());
    assert!(!HeightTier::Bare.shows_keybar());
}

#[test]
fn default_rail_visible_threshold_is_176() {
    assert!(!default_rail_visible(160));
    assert!(!default_rail_visible(175));
    assert!(default_rail_visible(176));
    assert!(default_rail_visible(199));
}

// ---------------------------------------------------------------------
// The L-floor arithmetic (§4.2's literal check)
// ---------------------------------------------------------------------

#[test]
fn l_floor_arithmetic_matches_the_spec() {
    // 160 - 2 (outer border) - 3 (three seams among four cards) - 22
    // (sidebar) - 34 (rail, as the constraint fed to the solver) = 99
    // *content* columns, split ~evenly between List and Reader. Every card
    // but Sidebar (the first) absorbs its own preceding seam as its own
    // leftmost column — see `split_with_separators`'s doc comment — so
    // List/Reader/Rail's *rendered* `Rect` widths are each one column wider
    // than their content share.
    let area = Rect::new(0, 0, 160, 40);
    let plan = layout_mode(area, ctx(Card::List));
    assert_eq!(plan.breakpoint, Breakpoint::L);

    let sidebar = plan.placement(Card::Sidebar).rect().expect("sidebar shown");
    let list = plan.placement(Card::List).rect().expect("list shown");
    let reader = plan.placement(Card::Reader).rect().expect("reader shown");
    let rail = plan.placement(Card::Rail).rect().expect("rail shown");

    // Sidebar is the first card: no seam to absorb, exactly its Length(22).
    assert_eq!(sidebar.width, 22);
    // Rail absorbs one seam: 34 content + 1 border column.
    assert_eq!(rail.width, 35);

    let list_content = list.width - 1;
    let reader_content = reader.width - 1;
    assert_eq!(
        list_content + reader_content,
        99,
        "list+reader content must sum to 99"
    );
    // Pinned to the actual solver output rather than a tolerance band that
    // would silently accept either assignment: a tie-break that flipped
    // which side gets the extra column should show up as a test diff, not
    // vanish inside a `49..=50` range check. tui.md §4.2's own example says
    // "List ≈ 49, Reader ≈ 50" — the `≈` licenses either order, and
    // ratatui's cassowary solver happens to give the *other* one here; both
    // fit inside §6.2's 40-55-column row budget either way.
    assert_eq!(list_content, 50, "list content share (pinned; see comment)");
    assert_eq!(
        reader_content, 49,
        "reader content share (pinned; see comment)"
    );

    // And every card's Rect actually tiles the inner (border-shrunk) area
    // with no gap and no overlap: widths sum exactly to inner.width.
    let inner_width = area.width - 2;
    assert_eq!(
        sidebar.width + list.width + reader.width + rail.width,
        inner_width
    );
}

// ---------------------------------------------------------------------
// Per-breakpoint placement rules
// ---------------------------------------------------------------------

#[test]
fn xs_shows_only_the_focused_card_full_bleed() {
    let area = Rect::new(0, 0, 79, 40);
    for &focus in &Card::ALL {
        let plan = layout_mode(area, ctx(focus));
        assert_eq!(plan.breakpoint, Breakpoint::Xs);
        for &card in &Card::ALL {
            let placement = plan.placement(card);
            if card == focus {
                assert!(
                    !placement.is_hidden(),
                    "{card:?} is focused and must be visible"
                );
                let rect = placement.rect().unwrap();
                // Full-bleed: the whole inner (border-shrunk) area.
                assert_eq!(rect.width, area.width - 2);
                assert_eq!(rect.height, area.height - 2);
            } else {
                assert!(placement.is_hidden(), "{card:?} must be hidden");
            }
        }
    }
}

#[test]
fn xs_sidebar_and_rail_are_drawers_list_and_reader_are_not() {
    let area = Rect::new(0, 0, 79, 40);
    let sidebar_plan = layout_mode(area, ctx(Card::Sidebar));
    assert!(sidebar_plan.placement(Card::Sidebar).is_drawer());
    let rail_plan = layout_mode(area, ctx(Card::Rail));
    assert!(rail_plan.placement(Card::Rail).is_drawer());
    let list_plan = layout_mode(area, ctx(Card::List));
    assert!(!list_plan.placement(Card::List).is_drawer());
    let reader_plan = layout_mode(area, ctx(Card::Reader));
    assert!(!reader_plan.placement(Card::Reader).is_drawer());
}

#[test]
fn s_reader_collapses_to_zero_until_a_row_is_opened() {
    let area = Rect::new(0, 0, 100, 40);
    let mut c = ctx(Card::List);
    c.reader_open = false;
    let plan = layout_mode(area, c);
    assert_eq!(plan.breakpoint, Breakpoint::S);
    assert!(plan.placement(Card::Reader).is_hidden());
    let list = plan.placement(Card::List).rect().unwrap();
    assert_eq!(list.height, area.height - 2, "list owns the whole area");
}

#[test]
fn s_reader_open_splits_list_and_reader_vertically() {
    let area = Rect::new(0, 0, 100, 40);
    let mut c = ctx(Card::List);
    c.reader_open = true;
    let plan = layout_mode(area, c);
    let list = plan.placement(Card::List).rect().unwrap();
    let reader = plan.placement(Card::Reader).rect().unwrap();
    assert!(list.height >= 8, "list keeps its documented 8-row floor");
    assert_eq!(
        list.height + reader.height,
        area.height - 2,
        "list+reader fill the inner height exactly"
    );
    assert!(reader.y > list.y, "list is above reader (tig-style)");
}

#[test]
fn s_minimal_height_is_slide_between_not_a_stack() {
    let area = Rect::new(0, 0, 100, 18); // height 18 => Minimal tier
    let mut c = ctx(Card::List);
    c.height_tier = HeightTier::Minimal;

    c.reader_open = false;
    let closed = layout_mode(area, c);
    assert!(!closed.placement(Card::List).is_hidden());
    assert!(closed.placement(Card::Reader).is_hidden());

    c.reader_open = true;
    let open = layout_mode(area, c);
    assert!(open.placement(Card::List).is_hidden());
    assert!(!open.placement(Card::Reader).is_hidden());
}

#[test]
fn m_rail_is_always_a_drawer_never_in_the_split() {
    let area = Rect::new(0, 0, 140, 40);
    let mut c = ctx(Card::List);
    c.rail_visible = true;
    let plan = layout_mode(area, c);
    assert_eq!(plan.breakpoint, Breakpoint::M);
    assert!(
        plan.placement(Card::Rail).is_drawer(),
        "rail must be a drawer at M even when its pref is on"
    );
}

#[test]
fn m_sidebar_hidden_when_pref_off_and_unfocused_drawer_when_focused() {
    let area = Rect::new(0, 0, 140, 40);
    let mut c = ctx(Card::List);
    c.sidebar_visible = false;
    let unfocused = layout_mode(area, c);
    assert!(unfocused.placement(Card::Sidebar).is_hidden());

    c.focus = Card::Sidebar;
    let focused = layout_mode(area, c);
    assert!(focused.placement(Card::Sidebar).is_drawer());
}

#[test]
fn l_rail_joins_the_split_only_when_visible_otherwise_hidden_or_drawer() {
    let area = Rect::new(0, 0, 160, 40);
    let mut c = ctx(Card::List);

    c.rail_visible = true;
    let with_rail = layout_mode(area, c);
    assert!(matches!(
        with_rail.placement(Card::Rail),
        Placement::Shown(_)
    ));

    c.rail_visible = false;
    let without_rail_unfocused = layout_mode(area, c);
    assert!(without_rail_unfocused.placement(Card::Rail).is_hidden());

    c.focus = Card::Rail;
    let without_rail_focused = layout_mode(area, c);
    assert!(without_rail_focused.placement(Card::Rail).is_drawer());
}

#[test]
fn xl_is_structurally_identical_to_l_wide_case() {
    let l_area = Rect::new(0, 0, 199, 40);
    let xl_area = Rect::new(0, 0, 200, 40);
    let c = ctx(Card::List);
    let l_plan = layout_mode(l_area, c);
    let xl_plan = layout_mode(xl_area, c);
    assert_eq!(l_plan.breakpoint, Breakpoint::L);
    assert_eq!(xl_plan.breakpoint, Breakpoint::Xl);
    // Both place all four cards with the same fixed widths; only the two
    // Fill columns' exact split differs by one column at most.
    assert_eq!(
        l_plan.placement(Card::Sidebar).rect().unwrap().width,
        xl_plan.placement(Card::Sidebar).rect().unwrap().width
    );
    assert_eq!(
        l_plan.placement(Card::Rail).rect().unwrap().width,
        xl_plan.placement(Card::Rail).rect().unwrap().width
    );
}

#[test]
fn zoom_hides_every_other_card_and_fills_the_area() {
    let area = Rect::new(0, 0, 200, 50);
    let mut c = ctx(Card::List);
    c.zoom = Some(Card::Rail);
    let plan = layout_mode(area, c);
    for &card in &Card::ALL {
        let placement = plan.placement(card);
        if card == Card::Rail {
            let rect = placement.rect().expect("zoomed card is shown");
            assert_eq!(rect.width, area.width - 2);
            assert_eq!(rect.height, area.height - 2);
        } else {
            assert!(placement.is_hidden());
        }
    }
}

#[test]
fn zoom_wins_even_at_xs_where_only_one_card_would_show_anyway() {
    let area = Rect::new(0, 0, 60, 30);
    let mut c = ctx(Card::Sidebar);
    c.zoom = Some(Card::List);
    let plan = layout_mode(area, c);
    // Focus is Sidebar but zoom names List — List wins, proving zoom is
    // checked before the per-breakpoint focus rules, not blended with them.
    assert!(!plan.placement(Card::List).is_hidden());
    assert!(plan.placement(Card::Sidebar).is_hidden());
}

#[test]
fn zoom_wins_even_at_bare_height() {
    let area = Rect::new(0, 0, 200, 12); // height 12 => Bare tier
    let mut c = ctx(Card::List);
    c.height_tier = HeightTier::Bare;
    c.focus = Card::Sidebar;
    c.zoom = Some(Card::Rail);
    let plan = layout_mode(area, c);
    assert!(!plan.placement(Card::Rail).is_hidden());
    assert!(plan.placement(Card::Sidebar).is_hidden());
}

// ---------------------------------------------------------------------
// Bare height tier (< 15 rows): single card, at every breakpoint — not
// only at XS. §4.3's "Single card + status bar only" row applies
// regardless of width.
// ---------------------------------------------------------------------

#[test]
fn bare_height_collapses_to_a_single_card_at_every_breakpoint() {
    // One representative width per breakpoint, all at a Bare height (12).
    let widths = [
        (60, Breakpoint::Xs),
        (100, Breakpoint::S),
        (140, Breakpoint::M),
        (180, Breakpoint::L),
        (220, Breakpoint::Xl),
    ];
    for (width, expected_bp) in widths {
        let area = Rect::new(0, 0, width, 12);
        let mut c = ctx(Card::List);
        c.height_tier = HeightTier::Bare;
        let plan = layout_mode(area, c);
        assert_eq!(plan.breakpoint, expected_bp);
        assert_eq!(plan.height_tier, HeightTier::Bare);
        let visible: Vec<Card> = plan.visible().map(|(card, _)| card).collect();
        assert_eq!(
            visible,
            vec![Card::List],
            "only the focused card should be visible at Bare height, width={width}"
        );
    }
}

#[test]
fn bare_height_follows_focus_like_xs_does() {
    let area = Rect::new(0, 0, 160, 12); // L-width, Bare height
    let mut c = ctx(Card::Rail);
    c.height_tier = HeightTier::Bare;
    let plan = layout_mode(area, c);
    assert!(plan.placement(Card::Rail).is_drawer());
    assert!(plan.placement(Card::List).is_hidden());
    assert!(plan.placement(Card::Sidebar).is_hidden());
    assert!(plan.placement(Card::Reader).is_hidden());
}

// ---------------------------------------------------------------------
// The drawer contract: painted over, never reflowed around — except at
// XS/Bare, where there is no split underneath to preserve (module docs'
// carve-out).
// ---------------------------------------------------------------------

#[test]
fn a_drawer_never_changes_any_other_cards_rect_at_s_m_l_xl() {
    // One width per breakpoint that actually has a split (everything but
    // XS): focusing Sidebar summons it as a drawer; List/Reader/Rail's
    // Rects must be pixel-for-pixel identical whether or not that drawer is
    // showing.
    for width in [100u16, 140, 160, 200] {
        let area = Rect::new(0, 0, width, 40);
        let mut c = ctx(Card::List);
        c.sidebar_visible = false;

        let unfocused = layout_mode(area, c);
        c.focus = Card::Sidebar;
        let focused = layout_mode(area, c);

        assert_eq!(
            unfocused.placement(Card::List).rect(),
            focused.placement(Card::List).rect(),
            "width={width}"
        );
        assert_eq!(
            unfocused.placement(Card::Reader).rect(),
            focused.placement(Card::Reader).rect(),
            "width={width}"
        );
        assert_eq!(
            unfocused.placement(Card::Rail).rect(),
            focused.placement(Card::Rail).rect(),
            "width={width}"
        );
    }
}

#[test]
fn at_xs_summoning_a_drawer_necessarily_hides_the_previous_card() {
    // The documented carve-out: at XS there is only one card's worth of
    // room, period, so focusing Sidebar *does* change what List shows —
    // there is nothing else it could do. This is the inverse of the
    // property above, checked explicitly so a future change that
    // "fixes" XS to match S/M/L/XL is caught as a spec violation, not
    // welcomed as a consistency improvement.
    let area = Rect::new(0, 0, 79, 40);
    let mut c = ctx(Card::List);
    let list_focused = layout_mode(area, c);
    assert!(!list_focused.placement(Card::List).is_hidden());

    c.focus = Card::Sidebar;
    let sidebar_focused = layout_mode(area, c);
    assert!(
        sidebar_focused.placement(Card::List).is_hidden(),
        "at XS, summoning the sidebar drawer must hide List — there is no \
         room for both"
    );
}

// ---------------------------------------------------------------------
// `DeckPlan`'s own surface: focus ring and carried-through height tier.
// ---------------------------------------------------------------------

#[test]
fn focus_ring_is_the_fixed_left_to_right_order() {
    let area = Rect::new(0, 0, 160, 40);
    let plan = layout_mode(area, ctx(Card::List));
    assert_eq!(
        plan.focus_ring(),
        [Card::Sidebar, Card::List, Card::Reader, Card::Rail]
    );
}

#[test]
fn deck_plan_carries_the_height_tier_it_was_computed_from() {
    let area = Rect::new(0, 0, 160, 20); // Compact tier
    let mut c = ctx(Card::List);
    c.height_tier = HeightTier::Compact;
    let plan = layout_mode(area, c);
    assert_eq!(plan.height_tier, HeightTier::Compact);
    assert!(plan.height_tier.lens_strip_folds_into_list_title());
}

// ---------------------------------------------------------------------
// Property sweep: no overflow, no degenerate (zero-size) `Shown`/`Drawer`
// placements, and no overlap between real split members, at every width
// 20..400 and every height 10..100 (Appendix A's normative range, craft
// rule §18.2) — with `height_tier` derived from the swept height, the way
// a real caller (`Model`) would, not hardcoded independently of it.
// ---------------------------------------------------------------------

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

/// A `DeckContext` builder for the sweep, everything but `height_tier` and
/// `focus` fixed by the caller — `height_tier` is filled in per-iteration
/// from the swept height, never hardcoded, so Bare/Minimal are actually
/// exercised at the heights that produce them.
type CtxFixture = fn(Card, HeightTier) -> DeckContext;

const FIXTURES: &[CtxFixture] = &[
    |focus, height_tier| DeckContext {
        focus,
        zoom: None,
        sidebar_visible: true,
        rail_visible: true,
        reader_open: true,
        height_tier,
    },
    |focus, height_tier| DeckContext {
        focus,
        zoom: None,
        sidebar_visible: false,
        rail_visible: false,
        reader_open: false,
        height_tier,
    },
    // The drawer paths at M/L/XL: hidden-by-pref but focused, for both
    // Sidebar and Rail — `ctx()`'s default (both prefs true) never
    // exercises `left_slice`/`right_slice` at all in the main sweep loop
    // below, since a pref-visible card is never a drawer.
    |focus, height_tier| DeckContext {
        focus,
        zoom: None,
        sidebar_visible: false,
        rail_visible: true,
        reader_open: true,
        height_tier,
    },
    |focus, height_tier| DeckContext {
        focus,
        zoom: None,
        sidebar_visible: true,
        rail_visible: false,
        reader_open: true,
        height_tier,
    },
    |focus, height_tier| DeckContext {
        focus,
        zoom: Some(Card::Reader),
        sidebar_visible: true,
        rail_visible: true,
        reader_open: true,
        height_tier,
    },
];

/// Every width the sweep below checks, chosen by construction rather than
/// exhaustion: every breakpoint boundary and its immediate neighbor on each
/// side (79/80, 119/120, 159/160, 199/200 ± 1), plus the swept range's two
/// extremes and one interior sample per breakpoint. `layout_mode` is
/// piecewise-linear in width *within* a breakpoint (`Length`/`Fill`
/// constraints, `min`/`saturating_sub`, no branch on width itself once the
/// breakpoint is chosen) — so a defect that held at both ends of a band and
/// broke strictly inside it, with no boundary condition anywhere near the
/// break, is not a class of bug this codebase's arithmetic can produce.
/// Testing every value in a linear band buys nothing an endpoint-plus-
/// sample doesn't already prove; it was measured (exhaustively, once, by
/// hand) to cost 25x the runtime for zero additional failures caught. See
/// `HEIGHTS` below for the same reasoning on the other axis.
const WIDTHS: &[u16] = &[
    20, 79, 80, 81, 100, 119, 120, 121, 140, 159, 160, 161, 180, 199, 200, 201, 300, 399,
];

/// Every height tier boundary and its neighbor (14/15, 19/20, 24/25,
/// 39/40 ± 1), the swept range's extremes, and one interior sample per
/// tier — the height-axis half of [`WIDTHS`]'s reasoning.
const HEIGHTS: &[u16] = &[10, 12, 14, 15, 16, 19, 20, 22, 24, 25, 30, 39, 40, 60, 99];

#[test]
fn no_overflow_no_degenerate_size_and_no_overlap_between_shown_cards_across_the_full_matrix() {
    // Drawers are *designed* to overlap a Shown card (§4.4 — painted over,
    // not reflowed around), so overlap is only checked among the `Shown`
    // (real split) members; every visible placement, Shown or Drawer, must
    // still stay within the outer area and have positive size — a
    // `Placement::Shown`/`Drawer` with a zero-size `Rect` is exactly the
    // "silently vanishes" this module's contract forbids (it reports
    // "visible" but renders nothing).
    for &width in WIDTHS {
        for &height in HEIGHTS {
            let area = Rect::new(0, 0, width, height);
            let tier = height_tier(height);
            for &fixture in FIXTURES {
                for &focus in &Card::ALL {
                    let c = fixture(focus, tier);
                    let plan = layout_mode(area, c);
                    assert_eq!(plan.height_tier, tier);

                    let visible: Vec<(Card, Rect)> = plan
                        .visible()
                        .filter_map(|(card, p)| p.rect().map(|r| (card, r)))
                        .collect();

                    for &(card, rect) in &visible {
                        assert!(
                            contains(area, rect),
                            "{card:?} at w={width} h={height} overflows: {rect:?} not in {area:?}"
                        );
                        assert!(
                            rect.width > 0 && rect.height > 0,
                            "{card:?} at w={width} h={height} tier={tier:?} is \
                             reported visible with a degenerate size: {rect:?}"
                        );
                    }

                    let shown: Vec<(Card, Rect)> = Card::ALL
                        .into_iter()
                        .filter_map(|card| match plan.placement(card) {
                            Placement::Shown(r) => Some((card, r)),
                            _ => None,
                        })
                        .collect();
                    for i in 0..shown.len() {
                        for j in (i + 1)..shown.len() {
                            let (card_a, rect_a) = shown[i];
                            let (card_b, rect_b) = shown[j];
                            assert!(
                                !rects_overlap(rect_a, rect_b),
                                "{card_a:?} {rect_a:?} overlaps {card_b:?} {rect_b:?} \
                                 at w={width} h={height}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn card_all_lists_every_variant_exactly_once() {
    // Exhaustive match over `Card`, independent of `Card::ALL`'s own
    // definition: adding a fifth variant without updating both `canonical`
    // and `Card::ALL` fails to *compile* here, not just fails silently at
    // run time.
    fn canonical(c: Card) -> usize {
        match c {
            Card::Sidebar => 0,
            Card::List => 1,
            Card::Reader => 2,
            Card::Rail => 3,
        }
    }
    let mut seen = [false; 4];
    for c in Card::ALL {
        let idx = canonical(c);
        assert!(!seen[idx], "{c:?} appears more than once in Card::ALL");
        seen[idx] = true;
    }
    assert!(seen.iter().all(|&s| s), "Card::ALL is missing a variant");
}
