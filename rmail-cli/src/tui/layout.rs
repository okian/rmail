//! The card/deck router — tui.md §2.2.1, §4.2, §4.3.
//!
//! [`layout_mode`] is the single source of truth for which of the four
//! persistent cards (Sidebar/List/Reader/Rail) are on screen at a given
//! terminal size, where each one's [`Rect`] is, and which ones are ephemeral
//! drawers rather than part of the normal split. Every later rendering task
//! and every behavior decision that needs to know "is the sidebar visible
//! right now" calls this function rather than keeping a second opinion —
//! tui.md's own words: "Rendering and behavior both consult it (never two
//! opinions)."
//!
//! # Why this module knows nothing about `Model`
//!
//! Nothing here imports `super::model`. `layout_mode` takes a [`DeckContext`]
//! of plain values instead of a `&Model` so it stays a pure geometry
//! function callable from a unit test with no daemon, no keymap, and no
//! terminal attached — the same reason `tui::model::update` stays free of
//! ratatui. [`DeckContext`]'s fields are resolved answers ("is the sidebar
//! visible", "does the reader have an open message"), not raw preferences;
//! *deciding* those answers from `tui.toml` defaults, `\`/`C-b` toggles, and
//! focus history is later tasks' job (109, 114, 132). This module only
//! answers "given these facts, where do the cards go."
//!
//! # Two separate concerns, two separate functions
//!
//! tui.md §4.2 (width breakpoints, which cards exist) and §4.3 (height
//! tiers, which chrome rows exist) are genuinely different axes — a
//! terminal can be S-wide and Full-height, or XL-wide and Bare-height. They
//! are kept as two independently testable pieces: [`height_tier`] (plus
//! [`HeightTier`]'s chrome-eligibility methods) answers §4.3 questions about
//! rows outside the card deck (header, lens strip, keybar); [`layout_mode`]
//! answers §4.2 questions about the deck itself, and takes an already-
//! resolved [`HeightTier`] only for the one place the two axes interact —
//! the S-breakpoint's stacked List/Reader collapsing to slide-between
//! behavior at the two shortest tiers (§4.3's S-stacking row).
//!
//! # Drawers are painted over, not reflowed around — at every breakpoint
//! # that has a split to paint over
//!
//! §4.4 describes a drawer as appearing "over the deck" and disappearing
//! when focus moves away — the mobile/web drawer pattern, not a reflow.
//! At every breakpoint from S upward, [`layout_mode`] takes that literally:
//! a card placed as [`Placement::Drawer`] does not change any other card's
//! [`Rect`]. The caller renders the normal split first, then paints the
//! drawer's `Rect` on top (with `Clear`, matching how this crate already
//! paints toasts and overlays over a rendered frame).
//!
//! **This does not extend to [`Breakpoint::Xs`] or [`HeightTier::Bare`]**,
//! and cannot: at those sizes only one card fits *at all* (§4.2's XS row,
//! §4.3's `<15` row), so summoning Sidebar or Rail as a drawer necessarily
//! hides whatever was showing before — there is no split underneath left to
//! preserve. "Same key, same meaning, identical at every width" (§4.4) is
//! still true in the sense that matters: `h`/`l`/`Tab`/`\`/`C-b` are the
//! same mechanism everywhere, and the *result* differs only because the
//! available space does.
//!
//! # Why this whole module is `#[allow(dead_code)]` for now
//!
//! Nothing outside this module and its own tests calls `layout_mode` yet —
//! `view.rs` still renders the v1 three-screen layout, and wiring this
//! router into it is Part VI task 120 (the outer grid/header/banner) and
//! task 109 (zoom + drawer state in `Model`), neither landed yet. Every
//! public item here is real, tested, production code the moment those tasks
//! consume it; it is exactly the "declared shape a named future task
//! consumes" pattern task 92 already established for `Toast::Completion`/
//! `Toast::Priority`, not a stub. Delete this allow in task 120's diff, when
//! `render` finally calls into this module and `dead_code` has something to
//! say again.
#![allow(dead_code)]

use ratatui::layout::{Constraint, Layout};
// Re-exported (not just used internally) so `tui::model` — which otherwise
// imports no ratatui type, per this crate's "one ratatui-aware module"
// invariant — can name `layout::Rect` in `Model::deck_plan`'s signature
// without reaching past this module into `ratatui` itself. `Rect` is bare
// geometry (four integers), not rendering; `Constraint`/`Layout` stay
// private since nothing outside this module needs to run the solver.
pub use ratatui::layout::Rect;

#[cfg(test)]
mod tests;

/// One of the four persistent cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Card {
    Sidebar,
    List,
    Reader,
    Rail,
}

impl Card {
    /// Left-to-right deck order, matching the order tui.md's frame diagrams
    /// draw them in (§3.1's `[1] SIDEBAR [2] LIST [3] READER [4] RAIL`).
    pub const ALL: [Card; 4] = [Card::Sidebar, Card::List, Card::Reader, Card::Rail];

    /// The lowercase name this card is called by in status text and pane
    /// titles — this crate's convention for both (see e.g. `pane_block`'s
    /// callers in `view.rs`: `"preview"`, `"message"`, `"manual"`). A method
    /// rather than a derived `Debug` string, so a rename of the variant
    /// cannot silently rewrite what a user reads — the same reason
    /// `Action::describe` and `Mode::id` are hand-written rather than
    /// derived.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Card::Sidebar => "sidebar",
            Card::List => "list",
            Card::Reader => "reader",
            Card::Rail => "rail",
        }
    }
}

/// Width breakpoint, named for tui.md §4.2's own vocabulary so a reader can
/// grep the spec section by the variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breakpoint {
    /// `< 80`. One card full-bleed; Sidebar/Rail summon as full-frame
    /// drawers; Reader auto-zooms on Enter.
    Xs,
    /// `80..=119`. List over Reader, stacked vertically (tig-style).
    S,
    /// `120..=159`. Sidebar │ List │ Reader; Rail is always a right drawer.
    M,
    /// `160..=199`. All four cards; Rail joins the split only at `>= 176`.
    L,
    /// `>= 200`. Structurally identical to L's wide case.
    Xl,
}

/// Classify a raw terminal width into its tui.md §4.2 breakpoint.
pub fn breakpoint(width: u16) -> Breakpoint {
    match width {
        0..=79 => Breakpoint::Xs,
        80..=119 => Breakpoint::S,
        120..=159 => Breakpoint::M,
        160..=199 => Breakpoint::L,
        _ => Breakpoint::Xl,
    }
}

/// tui.md §4.2: at the L breakpoint, whether Rail joins the split by
/// default depends on the exact width, not merely the breakpoint — `>= 176`
/// on, `160..=175` off. A caller (task 114's prefs, or an explicit user
/// toggle) may override this; this is only the *documented default*.
///
/// At M and XS the rail is never in the split by default (M: always a
/// drawer; XS: only the focused card shows at all) — this function is only
/// meaningful once the caller has already established the width is in L's
/// range or wider, so it does not itself branch on [`Breakpoint`].
pub fn default_rail_visible(width: u16) -> bool {
    width >= 176
}

/// Height tier, named for tui.md §4.3's own vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightTier {
    /// `>= 40` rows. Full chrome; time-bucket section headers eligible.
    Full,
    /// `25..=39` rows. Header gauges prune to glyph-only.
    Reduced,
    /// `20..=24` rows. Lens strip folds into the list title; keybar
    /// dropped; which-key band capped at 1 row.
    Compact,
    /// `15..=19` rows. Header dropped entirely (its gauges fold into the
    /// status bar's daemon zone); S-breakpoint stacking becomes
    /// slide-between.
    Minimal,
    /// `< 15` rows. Single card plus the status bar; overlays render
    /// full-frame.
    Bare,
}

/// Classify a raw terminal height into its tui.md §4.3 tier.
pub fn height_tier(height: u16) -> HeightTier {
    match height {
        0..=14 => HeightTier::Bare,
        15..=19 => HeightTier::Minimal,
        20..=24 => HeightTier::Compact,
        25..=39 => HeightTier::Reduced,
        _ => HeightTier::Full,
    }
}

impl HeightTier {
    /// Whether the header band (row 1) renders at all. Dropped only at
    /// [`HeightTier::Minimal`] and below, per §4.3's "Header dropped" row —
    /// its gauges relocate into the status bar's daemon zone rather than
    /// vanishing (law 5, no invisible state), which is a rendering detail
    /// for the header/status tasks, not this classification.
    pub fn shows_header(self) -> bool {
        !matches!(self, HeightTier::Minimal | HeightTier::Bare)
    }

    /// Whether the header's gauges render in their full labeled form
    /// (`SYNC ✓ 8s ago`) versus glyph-only (`✓`) — full chrome only at
    /// [`HeightTier::Full`], per §4.3's narrowing row.
    pub fn header_gauges_glyph_only(self) -> bool {
        !matches!(self, HeightTier::Full)
    }

    /// Whether the lens strip / breadcrumb row renders as its own row.
    /// Dropped below 25 rows; at [`HeightTier::Compact`] specifically it
    /// folds into the list title instead of disappearing outright — see
    /// [`HeightTier::lens_strip_folds_into_list_title`].
    pub fn shows_lens_strip(self) -> bool {
        matches!(self, HeightTier::Full | HeightTier::Reduced)
    }

    /// Whether the dropped lens strip's content folds into the list card's
    /// title line rather than being lost — true only at
    /// [`HeightTier::Compact`] (20–24 rows); below that there is no list
    /// title to fold into either, in the same sense the whole chrome
    /// degrades further.
    pub fn lens_strip_folds_into_list_title(self) -> bool {
        matches!(self, HeightTier::Compact)
    }

    /// Whether the keybar (bottom hint row) renders. Dropped below 25 rows,
    /// same threshold as the lens strip.
    pub fn shows_keybar(self) -> bool {
        matches!(self, HeightTier::Full | HeightTier::Reduced)
    }

    /// The most rows the which-key band may occupy at this tier. §4.3 only
    /// states this cap for [`HeightTier::Compact`] (1 row) versus
    /// everything above it (2); below Compact the whole card area has
    /// collapsed to a single card and a caller should not really be asking
    /// this question — but a defined answer (the tightest cap) is returned
    /// rather than treating "below Compact" as unreachable, since "no
    /// invisible state" applies to this module's own contract too: a case
    /// that shouldn't come up still gets an honest value instead of an
    /// arbitrary one.
    pub fn whichkey_row_cap(self) -> u16 {
        match self {
            HeightTier::Full | HeightTier::Reduced => 2,
            HeightTier::Compact | HeightTier::Minimal | HeightTier::Bare => 1,
        }
    }

    /// Whether time-bucket section headers (§6.5) are eligible at all by
    /// height tier alone. §6.5 additionally requires >= 30 *terminal* rows
    /// and a date-sorted list — the row-count half of that is content
    /// policy the list-rendering task owns (it has the real row budget this
    /// module does not), so this only rules out the tiers where buckets are
    /// never eligible regardless of row count.
    pub fn time_buckets_eligible(self) -> bool {
        matches!(self, HeightTier::Full | HeightTier::Reduced)
    }

    /// Whether the S-breakpoint's List-over-Reader stack is replaced by
    /// slide-between (open replaces list; Esc returns) rather than showing
    /// both at once — true only at [`HeightTier::Minimal`], per §4.3's
    /// "S-stacking replaced by slide-between" row. Irrelevant at other
    /// breakpoints; [`layout_mode`] only consults this when `breakpoint(..)`
    /// is [`Breakpoint::S`].
    pub fn s_stack_becomes_slide_between(self) -> bool {
        matches!(self, HeightTier::Minimal)
    }
}

/// Where a card renders, or doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Not on screen at all right now.
    Hidden,
    /// Part of the normal card split — other cards' `Rect`s are computed
    /// around it.
    Shown(Rect),
    /// An ephemeral drawer painted over the normal split (§4.4). The
    /// `Rect`s of every other [`Placement::Shown`] card are exactly what
    /// they would be if this drawer did not exist.
    Drawer(Rect),
}

impl Placement {
    /// The `Rect` this card occupies, if it is on screen at all — the
    /// common case a caller wants ("where do I draw this, if anywhere").
    pub fn rect(self) -> Option<Rect> {
        match self {
            Placement::Hidden => None,
            Placement::Shown(r) | Placement::Drawer(r) => Some(r),
        }
    }

    pub fn is_hidden(self) -> bool {
        matches!(self, Placement::Hidden)
    }

    pub fn is_drawer(self) -> bool {
        matches!(self, Placement::Drawer(_))
    }
}

/// Already-resolved facts [`layout_mode`] needs to place the four cards.
///
/// Every field is an *answer*, not a preference — see the module's own
/// docs on why this type carries no knowledge of `tui.toml` or `Model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeckContext {
    /// The card that currently has keyboard focus.
    pub focus: Card,
    /// A card the user has zoomed full-bleed (`Z`), if any. Sticky across
    /// resizes and focus changes at the `Model` level — this function just
    /// applies it for one frame.
    pub zoom: Option<Card>,
    /// Whether the sidebar is part of the normal split at breakpoints that
    /// can afford it (M/L/XL). Already resolved from `tui.toml` + any `C-b`
    /// toggle this session.
    pub sidebar_visible: bool,
    /// Whether the rail is part of the normal split at the L/XL breakpoint
    /// (M never puts it in the split; see [`default_rail_visible`] for the
    /// documented default at L). Already resolved from `tui.toml` + `\`.
    pub rail_visible: bool,
    /// Whether the focused collection currently has an open message/row in
    /// the Reader. Drives two things at the S breakpoint: whether the
    /// Reader's row collapses to 0 height (§4.2's S row) and, at
    /// [`HeightTier::Minimal`], which of List/Reader the slide-between
    /// shows (§4.3's S-stacking row).
    pub reader_open: bool,
    /// The already-classified height tier ([`height_tier`] applied to the
    /// real terminal height) — needed only for the S/Minimal interaction
    /// documented on [`HeightTier::s_stack_becomes_slide_between`].
    pub height_tier: HeightTier,
}

/// One frame's answer to "where do the four cards go."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeckPlan {
    pub breakpoint: Breakpoint,
    /// Carried through from the [`DeckContext`] this plan was computed
    /// from, so a caller holding only the `DeckPlan` (not the original
    /// `ctx`) can still answer "what did this frame's chrome shed" — §4.3's
    /// drop-order tables and the [`HeightTier`] methods that implement them
    /// are otherwise unreachable from the plan alone.
    pub height_tier: HeightTier,
    sidebar: Placement,
    list: Placement,
    reader: Placement,
    rail: Placement,
}

impl DeckPlan {
    /// This card's placement for the frame this plan was computed for.
    pub fn placement(&self, card: Card) -> Placement {
        match card {
            Card::Sidebar => self.sidebar,
            Card::List => self.list,
            Card::Reader => self.reader,
            Card::Rail => self.rail,
        }
    }

    /// Every card currently on screen (shown or drawer), for a caller that
    /// wants to iterate rather than ask about one card at a time — e.g. "is
    /// anything covering the deck as a drawer right now."
    pub fn visible(&self) -> impl Iterator<Item = (Card, Placement)> + '_ {
        Card::ALL
            .into_iter()
            .map(|c| (c, self.placement(c)))
            .filter(|(_, p)| !p.is_hidden())
    }

    /// The fixed order `h`/`l`/`Tab` cycle the four cards in, left to right
    /// per §3.1's frame diagram. Not a function of `self` today — nothing in
    /// tui.md makes focus order vary by breakpoint or zoom state (§4.4: "same
    /// key, same meaning, identical at every width") — but it is a method
    /// on the plan rather than a bare `Card::ALL` reference at each call
    /// site, so a future rule that *did* make the ring context-sensitive
    /// would have exactly one place to change.
    pub fn focus_ring(&self) -> [Card; 4] {
        Card::ALL
    }
}

/// Reduce `area` by `margin` columns/rows on every side, saturating at
/// zero rather than panicking on a `Rect` smaller than the margin.
///
/// `Rect` does have its own `inner(Margin)` in this ratatui version — but it
/// collapses to `Rect::ZERO` (origin `0,0`) when the margin doesn't fit,
/// while this saturates width/height to zero and *keeps* `x`/`y` at their
/// real screen position. That distinction is load-bearing here:
/// [`left_slice`]/[`right_slice`] position a drawer relative to `inner.x`/
/// `inner.y`, and a drawer that silently jumped to the terminal's absolute
/// origin on a two-column-wide terminal would be a worse bug than the
/// degenerate size itself.
fn shrink(area: Rect, margin: u16) -> Rect {
    Rect {
        x: area.x.saturating_add(margin),
        y: area.y.saturating_add(margin),
        width: area.width.saturating_sub(margin.saturating_mul(2)),
        height: area.height.saturating_sub(margin.saturating_mul(2)),
    }
}

/// Split `area` horizontally into cards, each with `card_constraints[i]`
/// columns of *content* width, separated by the single-column seam every
/// card but the first draws its own `Borders::LEFT` glyph into (§4.1's
/// "collapsed borders" rule: "separators are single `│` runs", contributed
/// by the card to their right, not a gap owned by nobody).
///
/// The seam is a real `Length(1)` slot in the constraint solve — so `Fill`
/// constraints only compete for what's left after every fixed width *and*
/// every seam is satisfied, which is the literal mechanism behind §4.2's
/// arithmetic check (`160 − 5 − 22 − 34 = 99`: the outer 2 columns are
/// `shrink`'s job, the `N - 1` seam columns are this function's) — but
/// unlike a plain "split and discard the gaps" implementation, each seam is
/// then **merged into the following card's `Rect`** as that card's own
/// leftmost column, rather than left unassigned. A returned `Rect`'s width
/// is therefore `card_constraints[i]` for the first card and
/// `card_constraints[i] + 1` for every other one: the caller draws
/// `Borders::LEFT` directly on the `Rect` as given and gets `.inner()` back
/// to exactly `card_constraints[i]` content columns, with no separate
/// "which cards need an extra column of border reach" bookkeeping.
fn split_with_separators(area: Rect, card_constraints: &[Constraint]) -> Vec<Rect> {
    if card_constraints.is_empty() {
        return Vec::new();
    }
    let mut constraints = Vec::with_capacity(card_constraints.len() * 2 - 1);
    for (i, c) in card_constraints.iter().enumerate() {
        if i > 0 {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(*c);
    }
    let split = Layout::horizontal(constraints).split(area);

    let mut rects = Vec::with_capacity(card_constraints.len());
    for i in 0..card_constraints.len() {
        let card_idx = i * 2;
        if i == 0 {
            rects.push(split[card_idx]);
        } else {
            let seam = split[card_idx - 1];
            let card = split[card_idx];
            rects.push(Rect {
                x: seam.x,
                y: card.y,
                width: seam.width + card.width,
                height: card.height,
            });
        }
    }
    rects
}

/// Sidebar's split width at M/L/XL, per §4.2's table.
const SIDEBAR_WIDTH: u16 = 22;
/// Rail's split width at L/XL (and its drawer width everywhere), per §4.2.
const RAIL_WIDTH: u16 = 34;
/// Rail's drawer width at M, and the drawer width used by any card summoned
/// as a drawer while its home breakpoint doesn't afford it. tui.md gives
/// the sidebar drawer 24 and the rail drawer 34 (§4.4); both are used
/// verbatim below rather than reusing [`SIDEBAR_WIDTH`]/[`RAIL_WIDTH`],
/// since a drawer is explicitly a *different* width than the in-split
/// column at M/L for the sidebar (`Length(22)` in-split vs `Length(24)`
/// drawer).
const SIDEBAR_DRAWER_WIDTH: u16 = 24;

/// The single source of truth for where the four cards render.
///
/// `area` is the raw Fill(1) row tui.md §4.1 reserves for "Cards (or
/// full-frame app)" — before any border has been subtracted. This function
/// reserves the outer rounded-border margin itself (§4.1's "the frame draws
/// one outer `Rounded` border") before splitting.
pub fn layout_mode(area: Rect, ctx: DeckContext) -> DeckPlan {
    let bp = breakpoint(area.width);
    let inner = shrink(area, 1);

    // Zoom wins over every other rule, including height: the focused card
    // takes the whole inner area, full-bleed, and nothing else is on
    // screen — §4.5. (A tiny terminal is exactly where zooming the one
    // card you care about is most useful, not a state that should disable
    // the key.)
    if let Some(zoomed) = ctx.zoom {
        return DeckPlan {
            breakpoint: bp,
            height_tier: ctx.height_tier,
            sidebar: hidden_unless(Card::Sidebar, zoomed, inner),
            list: hidden_unless(Card::List, zoomed, inner),
            reader: hidden_unless(Card::Reader, zoomed, inner),
            rail: hidden_unless(Card::Rail, zoomed, inner),
        };
    }

    // §4.3's `< 15` row is "Single card + status bar only" — a rule about
    // *height* that applies regardless of width breakpoint, not an
    // S-breakpoint-only interaction like slide-between. Every breakpoint's
    // own function below is otherwise blind to `ctx.height_tier` (M/L/XL
    // never consult it at all), so this has to be resolved before the
    // breakpoint dispatch, not inside one arm of it.
    if ctx.height_tier == HeightTier::Bare {
        return single_card(bp, inner, ctx.focus, ctx.height_tier);
    }

    match bp {
        Breakpoint::Xs => layout_xs(inner, ctx),
        Breakpoint::S => layout_s(inner, ctx),
        Breakpoint::M => layout_m(inner, ctx),
        Breakpoint::L | Breakpoint::Xl => layout_l_xl(inner, ctx, bp),
    }
}

fn hidden_unless(card: Card, zoomed: Card, inner: Rect) -> Placement {
    if card == zoomed {
        Placement::Shown(inner)
    } else {
        Placement::Hidden
    }
}

/// One card, full-bleed — the focused one — with every other card either
/// hidden or, for Sidebar/Rail, conceptually a drawer even though its
/// `Rect` is identical to a `Shown` full-bleed card (there is nowhere else
/// for a drawer to go when only one card fits at all). List and Reader are
/// the "home" cards reached by normal navigation (master-detail promotion,
/// §3.2) rather than a drawer summon, so they are `Shown`.
///
/// Shared by two genuinely different callers for the same reason: §4.2's XS
/// breakpoint (`layout_xs`) and §4.3's Bare height tier (any breakpoint,
/// handled directly in [`layout_mode`]) independently arrive at "there is
/// only room for one card," and it is the same rule both times.
fn single_card(bp: Breakpoint, inner: Rect, focus: Card, height_tier: HeightTier) -> DeckPlan {
    let place = |card: Card| -> Placement {
        if focus != card {
            return Placement::Hidden;
        }
        match card {
            Card::Sidebar | Card::Rail => Placement::Drawer(inner),
            Card::List | Card::Reader => Placement::Shown(inner),
        }
    };
    DeckPlan {
        breakpoint: bp,
        height_tier,
        sidebar: place(Card::Sidebar),
        list: place(Card::List),
        reader: place(Card::Reader),
        rail: place(Card::Rail),
    }
}

/// §4.2's XS row: one card, full-bleed. See [`single_card`], which this
/// delegates to entirely — XS is "always exactly one card fits," the same
/// condition §4.3's Bare height tier reaches independently at any width.
fn layout_xs(inner: Rect, ctx: DeckContext) -> DeckPlan {
    single_card(Breakpoint::Xs, inner, ctx.focus, ctx.height_tier)
}

/// §4.2's S row: List over Reader, stacked (tig-style); Sidebar/Rail are
/// drawers painted over the stack. At [`HeightTier::Minimal`] (§4.3's
/// "S-stacking replaced by slide-between") only one of List/Reader shows at
/// a time instead of a vertical split.
fn layout_s(inner: Rect, ctx: DeckContext) -> DeckPlan {
    let (list, reader) = if ctx.height_tier.s_stack_becomes_slide_between() {
        if ctx.reader_open {
            (Placement::Hidden, Placement::Shown(inner))
        } else {
            (Placement::Shown(inner), Placement::Hidden)
        }
    } else if !ctx.reader_open {
        // Reader collapses to 0 until a row is opened (§4.2) — List simply
        // owns the whole area; no Fill contention to resolve.
        (Placement::Shown(inner), Placement::Hidden)
    } else {
        // List Fill(2), Reader Fill(3), with List floored at 8 rows when
        // the terminal is tall enough to afford the floor at all. Ratatui
        // 0.29's `Constraint` has no fluent "Fill, but at least N" — the
        // floor is applied as a manual correction after the proportional
        // split rather than expressed as one constraint.
        let rows = Layout::vertical([Constraint::Fill(2), Constraint::Fill(3)]).split(inner);
        let (list_rect, reader_rect) = if rows[0].height < 8 && inner.height >= 8 {
            let list_h = 8;
            let reader_h = inner.height.saturating_sub(list_h);
            (
                Rect {
                    height: list_h,
                    ..inner
                },
                Rect {
                    y: inner.y.saturating_add(list_h),
                    height: reader_h,
                    ..inner
                },
            )
        } else {
            (rows[0], rows[1])
        };
        (Placement::Shown(list_rect), Placement::Shown(reader_rect))
    };

    let sidebar = if ctx.focus == Card::Sidebar {
        Placement::Drawer(left_slice(inner, SIDEBAR_DRAWER_WIDTH))
    } else {
        Placement::Hidden
    };
    let rail = if ctx.focus == Card::Rail {
        Placement::Drawer(right_slice(inner, RAIL_WIDTH))
    } else {
        Placement::Hidden
    };

    DeckPlan {
        breakpoint: Breakpoint::S,
        height_tier: ctx.height_tier,
        sidebar,
        list,
        reader,
        rail,
    }
}

/// §4.2's M row: Sidebar │ List │ Reader in the split (sidebar gated on
/// `ctx.sidebar_visible`); Rail is *always* a drawer at this breakpoint,
/// never part of the split, whether shown by default or by focus.
fn layout_m(inner: Rect, ctx: DeckContext) -> DeckPlan {
    let show_sidebar = ctx.sidebar_visible;
    let constraints = [
        Constraint::Length(SIDEBAR_WIDTH),
        Constraint::Fill(5), // List
        Constraint::Fill(4), // Reader
    ];

    // When the sidebar isn't in the split, drop its constraint entirely
    // (not just hide the resulting Rect) so List/Reader's Fill weights
    // compete for the width it would have taken — "get the room back",
    // not "get a blank column".
    let rects = if show_sidebar {
        split_with_separators(inner, &constraints)
    } else {
        split_with_separators(inner, &constraints[1..])
    };

    let (sidebar_shown, list_rect, reader_rect) = if show_sidebar {
        (Some(rects[0]), rects[1], rects[2])
    } else {
        (None, rects[0], rects[1])
    };

    let sidebar = match (sidebar_shown, ctx.focus == Card::Sidebar) {
        (Some(r), _) => Placement::Shown(r),
        (None, true) => Placement::Drawer(left_slice(inner, SIDEBAR_DRAWER_WIDTH)),
        (None, false) => Placement::Hidden,
    };

    let rail_on = ctx.rail_visible || ctx.focus == Card::Rail;
    let rail = if rail_on {
        Placement::Drawer(right_slice(inner, RAIL_WIDTH))
    } else {
        Placement::Hidden
    };

    DeckPlan {
        breakpoint: Breakpoint::M,
        height_tier: ctx.height_tier,
        sidebar,
        list: Placement::Shown(list_rect),
        reader: Placement::Shown(reader_rect),
        rail,
    }
}

/// §4.2's L/XL rows: all four cards can be in the split at once. Sidebar
/// gated on `ctx.sidebar_visible` exactly as at M; Rail gated on
/// `ctx.rail_visible` — this function joins Rail to the split whenever that
/// flag is set, at *any* L/XL width, and never consults
/// [`default_rail_visible`] itself. The `>= 176` rule is only the
/// *documented default value* a caller should resolve `rail_visible` to
/// before calling this (from `tui.toml`, absent an explicit user override);
/// `layout_mode` has no opinion on where that bool came from. See
/// [`DeckContext::rail_visible`]'s docs.
fn layout_l_xl(inner: Rect, ctx: DeckContext, bp: Breakpoint) -> DeckPlan {
    let show_sidebar = ctx.sidebar_visible;
    let show_rail = ctx.rail_visible;

    let mut constraints = Vec::with_capacity(4);
    if show_sidebar {
        constraints.push(Constraint::Length(SIDEBAR_WIDTH));
    }
    constraints.push(Constraint::Fill(5)); // List
    constraints.push(Constraint::Fill(5)); // Reader
    if show_rail {
        constraints.push(Constraint::Length(RAIL_WIDTH));
    }

    let rects = split_with_separators(inner, &constraints);

    let mut idx = 0;
    let sidebar_rect = if show_sidebar {
        let r = rects[idx];
        idx += 1;
        Some(r)
    } else {
        None
    };
    let list_rect = rects[idx];
    idx += 1;
    let reader_rect = rects[idx];
    idx += 1;
    let rail_rect = if show_rail { Some(rects[idx]) } else { None };

    let sidebar = match (sidebar_rect, ctx.focus == Card::Sidebar) {
        (Some(r), _) => Placement::Shown(r),
        (None, true) => Placement::Drawer(left_slice(inner, SIDEBAR_DRAWER_WIDTH)),
        (None, false) => Placement::Hidden,
    };
    let rail = match (rail_rect, ctx.focus == Card::Rail) {
        (Some(r), _) => Placement::Shown(r),
        (None, true) => Placement::Drawer(right_slice(inner, RAIL_WIDTH)),
        (None, false) => Placement::Hidden,
    };

    DeckPlan {
        breakpoint: bp,
        height_tier: ctx.height_tier,
        sidebar,
        list: Placement::Shown(list_rect),
        reader: Placement::Shown(reader_rect),
        rail,
    }
}

fn left_slice(area: Rect, width: u16) -> Rect {
    Rect {
        width: width.min(area.width),
        ..area
    }
}

fn right_slice(area: Rect, width: u16) -> Rect {
    let width = width.min(area.width);
    Rect {
        x: area.x.saturating_add(area.width - width),
        width,
        ..area
    }
}
