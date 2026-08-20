# rmail TUI — Design Specification

**Codename: Cockpit.** A ground-up redesign of the `mail` TUI. This document is the
implementor's contract: every panel, key, color, state, and RPC wiring is specified here.
Where the daemon lacks a capability, that gap is stated honestly (§19) rather than papered
over — *trust is the product*.

> **Provenance.** This design was produced by a structured process: three independent
> full designs (a persistent-dashboard "Cockpit", a full-bleed "Monocle", and a responsive
> "Adaptive") were scored by three adversarial judges (daily-driver, implementor, design
> critic). Cockpit won unanimously; this document is Cockpit's frame with the judges'
> mandated grafts folded in — Monocle's typography, provenance marking, and teaching
> machinery; Adaptive's zoom, drawers, Esc law, and startup states — and every defect the
> judges found fixed (§1.3 lists the arbitrations).

**Relationship to the current TUI.** The current implementation (tasks 83–106) is
superseded as a *design* but not as an *architecture*: §2 lists the invariants that carry
over verbatim and the enumerated engineering delta. The Elm-style model, the command/verb
registry, parity testing, the Report/Form machinery, `terminal_safe`, and the stream
discipline are all retained — this redesign changes what the view draws and how
interaction feels, not what the model is.

---

## Table of contents

1. Design philosophy & laws
2. Architecture constitution (retained invariants + engineering delta)
3. Information architecture — frame, cards, collections, overlays, zoom, drawers
4. Layout system — breakpoints, height tiers, drop orders, borders, the Esc law
5. Panels, line by line — header, lens strip, sidebar, list, reader, rail, status, keybar
6. Message list row anatomy — glyphs, columns per breakpoint, dates, threading, marks
7. Reader — header block, body rendering, quotes, HTML, links, attachments, AI capsule
8. The keyboard model — laws, full keymaps, chord families, arbitration table, discoverability
9. Search
10. Filter vs Search vs Finder — one grammar, three engines
11. Sorting
12. Async, progress & honesty machinery
13. Color system (contrast-verified) & icon tiers
14. Compose
15. State walkthroughs — first run, trainer, initial sync, morning triage, offline, daemon down, auth states, empty/error states
16. Every remaining surface — ops, insights, automation, settings, help, accounts, tokens, ask
17. RPC wiring map (implementor appendix)
18. Performance budgets & craft rules
19. Daemon gaps & proposed RPCs
20. Deliberately not included
21. Migration notes from the current TUI

---

# 1. Design philosophy & laws

The cockpit shows the state of your entire mail world — sync, index, AI queue, spend,
outbox, lenses, folders, message, context — at every moment, in fixed places that never
move, so the eyes learn coordinates and the hands learn verbs. One frame, four cards;
the List card is polymorphic over *collections* (a folder, the unified inbox, search
results, the outbox, subscriptions, invoices…), so there is one list engine and one key
vocabulary for everything. Nothing "navigates away": you drill down a rendered breadcrumb
and `Esc` climbs back out. Every keystroke is a named action shared with `:` commands,
gRPC, and MCP; keys are macros for commands, so discoverability (which-key, keybar,
palette, help) is *generated*, never documented by hand. The daemon is the truth and the
TUI is a thin, optimistic, cancellable projection of it: act instantly, reconcile on
events, undo instead of confirm, and never block a frame on a network or a model.

## 1.1 The laws (testable, non-negotiable)

1. **Spatial stability.** The four cards keep their coordinates at every breakpoint that
   shows them. Chrome resigns before content does; nothing ever overflows a `Rect`.
2. **One meaning per key.** A key's meaning never changes across breakpoints, zoom, or
   drawer state — only its *target* (the focused card's selection) changes. The bounded
   exceptions live in the arbitration table (§8.3) and nowhere else.
3. **The Esc ladder** (§4.6) is a single ordered rule, implemented once, tested once.
4. **Overlays only add keys.** An overlay may introduce verbs; it may never rebind a core
   key to a different meaning.
5. **No invisible state.** Active filter, sort, lens, marks, mode, pending chord, inflight
   count, and daemon health are always written somewhere fixed (title/status/strip).
6. **Honesty over polish.** Loaded-row counts are labeled (`~`, `(partial)`, `• unvisited`);
   degraded search carries a badge; missing done-sentinels are reported as "cut short";
   provenance of model text is marked in characters, not just color.
7. **Optimistic + undoable beats confirmed.** Reversible actions apply instantly and
   reconcile via `WatchEvents`; destructive ones tier up (confirm → type-the-name).
8. **Never block, never blank.** Stale-while-revalidate everywhere; skeletons only where
   nothing was ever loaded; a keypress always paints within one frame.
9. **Model text is quoted.** Every model-generated string renders in the `ai` tint *and*
   between `«»` guillemets, so provenance survives `NO_COLOR`, the mono theme, and
   copy-paste. Parsed facts are never so marked.
10. **The daemon decodes; the client renders.** No MIME decoding, no HTML engine, no
    client re-threading of partially loaded folders, no invented counts.

## 1.2 What the judges mandated (grafts folded into this spec)

- `«»` provenance marking on all model text (§1.1 law 9, §7, §13).
- Reader measure clamp — `clamp(inner_width − 8, 72, 100)`, centered (§7.1).
- Contrast-verified fg ramp; floors enforced by a theme lint (§13).
- The single Esc precedence ladder + unbindable double-`Ctrl-C` quit (§4.6).
- `Z` zoom on any card; zoomed List *is* the headed sortable triage table (§4.5, §11).
- "Focus leads, layout follows" drawers replace per-panel overlay keys (§4.4).
- Vertical List-over-Reader stacking at 80–119 cols (§4.2).
- Lens tabs and sidebar queue rows render their own jump chords + counts (§5.2, §5.3).
- `''` last-lens flip (§8.2).
- One operator grammar for filter/search/saved/smart-folder; filter evaluates the
  client-safe subset and rejects the rest inline with "use / for that" (§10).
- Marks survive filtering with honest accounting: `3 marked (1 hidden)` (§6.6).
- Cursor never moves on network events; inserted rows pulse for 2 s (§12.8).
- Post-action teaching hints after three consecutive slow paths (§8.6).
- Time-bucket section headers on date-sorted lists at ≥30 rows (§6.5).
- Daemon-down screen `!` actually spawns `rmaild` and auto-reattaches (§15.7).
- Startup auth states: `local_login_required` gate, `RESOURCE_EXHAUSTED` lockout (§15.8).
- Server-side `thread_collapse` for search; no client re-threading of folders (§6.5, §9).
- Per-collection sort/density/rail state auto-persisted to `tui.toml` (§11, §2.2).
- Which-key overflow `+N more (?)` (§8.6).
- First-run key **trainer** (redesigned honest — §15.2 — no fake mailbox state).

## 1.3 Cockpit defects fixed in this synthesis

| Defect (judge-found) | Resolution |
|---|---|
| "digits are counts" vs `1` focusing sidebar | Digits are **always counts**. Card focus: `Tab`/`h`/`l`/`C-w`; sidebar toggle `C-b` (§8.2). |
| `h` = focus-left in list but pop in Reader | `h`/`l` are **always** card focus. Pop is `Esc`/`q` only. Reader is the promoted third card, so `h` from it lands on the list — same muscle motion, one rule (§3.2). |
| Rail AI tab: `a` both re-analyze and accept-tag | `a`/`x` accept/reject the **cursored suggestion row**; re-analyze is `!` (force) (§8.3). |
| Glyph column "5 cells" showing 4 glyphs | 1 mark cell + 4 glyph cells, priority-ordered (§6.1). |
| L-breakpoint (160 col) row budget overcommitted | Row budgets recomputed with arithmetic shown; tl_dr second line only at XL cozy (§6.2). |
| Reader wraps at 132 cols | Measure clamp, centered (§7.1). |
| `fg_muted` 2.8:1 | New ramp, computed: muted 7.0:1, faint 3.7:1 (§13). |
| Lens counts hand-waved | Honest count mechanism specified; no background searches by default (§5.2). |
| `z` was simultaneously zoom (graft) and fold prefix | **`Z` = zoom**; `z` is the vim-style fold/view chord (`za zq zs zd`) (§8.2). |
| Reader `s` star vs attachment `s` save | Attachment browser is an overlay; its row verbs are in the arbitration table (§8.3). |
| Two navigation systems (panel focus + drill stack) | Unified: Enter promotes focus rightward along master→detail; Esc pops the breadcrumb; cards never disappear into "decks" (§3). |

---

# 2. Architecture constitution

## 2.1 Retained invariants (from the shipped build — do not regress)

- **Elm-style model.** Pure, synchronous, clockless `update(Model, Msg) -> (Model, Vec<Cmd>)`;
  clock and terminal size arrive only as `Msg::Tick` / `Msg::Resize`. `Model::mode()` is
  derived, never stored.
- **One ratatui-aware module.** `view` is a pure `&Model → frame` function; no other module
  imports ratatui.
- **One vocabulary.** `Action` id namespace ≡ `:` verb grammar; keys and typed lines share
  one dispatch path (`run_verb`). Every bindable Action is in a parity capability or
  `LOCAL_ACTIONS`; the CI drift check stays.
- **Generated discoverability.** Help, which-key, keybar, palette annotations, and manual
  footers are generated from the live `Keymap` + verb registry; drift is a failing test.
- **Sanitization.** All untrusted text (subjects, bodies, model prose) passes
  `terminal_safe` (bidi controls, C0/C1, invisibles) before drawing.
- **Stream discipline.** Every stream is generation-stamped; supersession aborts on the
  executor side; daemon CANCELLED on a superseded stream is silence, not an error.
- **Thin client.** gRPC only — no SQLite, no IMAP, no filesystem state beyond
  `keys.toml`, `tui.toml`, history. Startup < 200 ms with the first frame painted before
  any RPC returns.
- **`keys.toml`** hot-reload (1 s poll), shadow lint, chord grammar; command history ring
  with the secret filter; `Report`/`Form`/`ConfigBlock` engines (§16 reuses them).

## 2.2 Engineering delta (new machinery this design requires)

Directly schedulable; each item is additive to the constitution above.

1. **Card/deck router.** `fn layout_mode(Rect) -> DeckPlan` — the single source of truth
   for which cards are visible, their Rects, drawer placements, and the focus ring.
   Rendering and behavior both consult it (never two opinions).
2. **Overlay stack** (max depth 3: e.g. confirm over picker over collection), replacing
   the single `Option<Overlay>` slot. Esc pops one. The which-key band is not an overlay.
3. **Collection engine.** The List card renders a `Collection` trait object: folder,
   unified, search-results, outbox, followups, waiting-on, notifications, subscriptions,
   invoices, rules, deliveries, audit, tokens… Each collection declares its columns
   (per-density), row verbs, title chips, and detail renderer (what the Reader card shows
   for its rows). One table engine, many resources (k9s model).
4. **Client unread ledger.** Per-folder unread estimates maintained from loaded rows +
   `WatchEvents` deltas; every derived count renders with `~` and unvisited folders show
   `•`, never a number (law 6).
5. **Filter engine.** Client-side predicate evaluation of the operator-grammar subset
   over loaded rows (§10).
6. **`tui.toml`** local prefs, write-through (debounced 1 s): theme, icon tier, density
   per collection-kind, sort per collection-kind, rail visibility + active tab, sidebar
   visibility, panel width overrides, hints on/off, mouse capture, undo-advance direction.
7. **Zoom + drawer state** in the model (per-card sticky zoom; at most one drawer).
8. **Undo stack.** Session-local inverse operations (move-back, unflag, untag,
   restore-from-trash, `CancelScheduled` in window) with idempotency keys. **No redo** —
   inverse-op redo over a drifting IMAP mailbox would lie.
9. **Lens engine.** Pinned queries with honest counts (§5.2).
10. **Wrapped-text cache** keyed `(message_id, width, fold_state)`; invalidated on resize.

---

# 3. Information architecture

## 3.1 The frame

One frame, always. The main area hosts the four cards; header, lens strip, status, and
keybar persist across everything except full-frame apps.

```
FRAME
├─ Header band     identity · gauges · session tally          (always; folds per §4.3)
├─ Lens strip      lens tabs ▸ or breadcrumb                  (≥25 rows)
├─ Cards
│   [1] SIDEBAR    accounts · mailboxes · queues · views · tags
│   [2] LIST       the active collection (polymorphic)
│   [3] READER     detail of the cursored row (message body, outbox entry, …)
│   [4] RAIL       context tabs: ✦AI · Thread · Ents · Contact · Why · Ask
├─ Transient rows  toast (0–1) · which-key band (0–2)         (only while live)
├─ Status bar      mode · scope · marks · message · undo chip · inflight · daemon
└─ Keybar          8 contextual hints                          (≥25 rows)

FULL-FRAME APPS (replace the cards, keep header+status; Esc returns)
  Compose · Settings · Manual · First-run · Trainer · Daemon-down screen

OVERLAY STACK (bordered = transient; Esc closes; max 3)
  finder ^P · command : · pickers (folder/tag/time/link) · attachment browser ·
  confirm · help ? · quick menu . · image viewer
```

## 3.2 Movement grammar

- **`Enter` promotes focus rightward along master→detail.** In a folder collection:
  list row → Reader card focused (full message rendered). On an outbox row → entry
  detail. On a citation → the cited message (pushes the breadcrumb).
- **`Esc`/`q` pop** (per the ladder, §4.6). The breadcrumb in the lens strip renders the
  navigation stack: `work ▸ INBOX ▸ ⧉ thread ▸ message 2/4`. No invisible state.
- **`h`/`l`/`Tab` move card focus.** If the target card is hidden at this breakpoint, it
  appears as an ephemeral **drawer** (§4.4) — focus leads, layout follows.
- **`Z` zooms** the focused card full-bleed (§4.5); the same component, more room.
- **`g` chords go somewhere** (§8.2); **`'` marks jump between collections**; **`:`/`^P`**
  reach everything by name. Collections remember cursor/scroll/sort across visits.
- Every surface in §16 is a collection reachable by `g`-chord, sidebar row, `:` verb, and
  finder — four routes, one action id.

## 3.3 Why not "decks"

The winning design had 9 deck screens that swapped the main area. The implementor judge
flagged two coexisting navigation systems; the fix (endorsed by all three judges' grafts)
is the polymorphic collection: **Ops/Insights/Automation surfaces are not separate
screens, they are collections in the same List+Reader cards** with their own columns and
row verbs. `go` (outbox) is exactly `gm` (mail) with a different collection loaded. This
halves the state machine and makes every future resource (a new daemon feature) a
~200-line `Collection` impl instead of a new screen.

---

# 4. Layout system

## 4.1 Grid

Outer vertical layout (`Layout::vertical`), destructured with `areas()`:

```
Length(1)    Header band
Length(0|1)  Banner row (offline / degradation) — reserved as Length(1) whenever any
             banner-worthy condition exists this session, so toggling never shifts rows
Length(1)    Lens strip / breadcrumb          (dropped <25 rows)
Fill(1)      Cards (or full-frame app)
Length(0..2) Which-key band (only while a prefix pends)
Length(1)    Status bar
Length(1)    Keybar                            (dropped <25 rows)
```

Toasts do **not** get a layout row: they float bottom-right over the card area
(`Clear` + 1-line block) so layout never shifts when a toast appears.

Cards: collapsed borders — the frame draws one outer `BorderType::Rounded` border; the
cards to the right of the first draw `Borders::LEFT` only, so separators are single `│`
runs. `Padding::horizontal(1)` everywhere; Reader adds vertical padding 1. **Focus is
shown by border/title color (accent), never border weight.** Section separators inside
cards are `─` rules in `border` color — never nested boxes.

**Border semantics (law):** a fully-enclosed rounded box is *always transient* — overlays,
pickers, confirms. If it has a closed border, Esc closes it. The persistent frame is the
only other bordered thing on screen.

## 4.2 Width breakpoints

One `layout_mode(area) -> DeckPlan`. S = sidebar, L = list, R = reader, C = rail.

| Breakpoint | Visible | Constraints (columns) |
|---|---|---|
| **XS < 80** | 1 card | Focused card `Fill(1)`. Reader auto-zooms on Enter. Sidebar/rail summon as full-frame drawers. |
| **S 80–119** | L over R, **stacked vertically** (tig-style) | List `Fill(2)` (min 8 rows), Reader `Fill(3)`; Reader row collapses to 0 until a row is opened. Sidebar/rail = drawers. |
| **M 120–159** | S │ L │ R | S `Length(22)` (toggle `C-b`), L `Fill(5)`, R `Fill(4)`. Rail = right drawer `Length(34)` on `\` or focus. |
| **L 160–199** | S │ L │ R │ C | S `Length(22)`, L `Fill(5)`, R `Fill(5)`, C `Length(34)`. Rail on by default ≥176, off 160–175. |
| **XL ≥ 200** | S │ L │ R │ C | As L; List defaults to cozy 2-line rows (§6.2). |

Arithmetic check at the L floor (160 cols): outer border 2 + 3 separators 3 = 5 chrome;
160−5−22−34 = 99 content split Fill(5)/Fill(5) → List ≈ 49, Reader ≈ 50. The §6.2 row
budget for 40–55-col lists is designed for exactly this. At 176+ the rail default-on
keeps List ≥ 56 (M-tier row layout). No declared minimum is unsatisfiable at any width
in its range (the Adaptive design's 126>120 bug class is checked by a unit test over
`layout_mode` at every width 20..400).

## 4.3 Height tiers

| Rows | Behavior |
|---|---|
| ≥ 40 | Full chrome. Time buckets on (§6.5). |
| 25–39 | Header gauges prune to glyph-only. Time buckets off <30. |
| 20–24 | Lens strip folds into the List title; keybar dropped; which-key capped at 1 row. |
| 15–19 | Header dropped; gauges fold into the status daemon zone; S-stacking replaced by slide-between (open replaces list; Esc returns). |
| < 15 | Single card + status bar only; overlays full-frame. |

**Drop orders are fixed and documented** (nothing silently vanishes; whatever a bar sheds
is one keypress away):

- Header, narrowing: verb labels → `NET` → `OUT` → `IDX` detail (dot stays) → session
  tally → clock.
- Status bar, narrowing: daemon glyph cluster → marks → scope; the message zone keeps its
  floor (`MIN_MESSAGE`); the undo-send chip is never dropped.
- List title, narrowing: live marker → shown/total → sort → filter chip (filter is the
  last to go; if it must go, the filter is still active and Esc still clears it — the
  status scope zone shows `</f>` as a 3-cell reminder).

## 4.4 Drawers — focus leads, layout follows

Focusing a card that the current breakpoint hides summons it as an **ephemeral drawer**
over the deck (sidebar: left, `Length(24)`; rail: right, `Length(34)`; at XS: full-frame).
Moving focus away closes it. There are no separate "open sidebar overlay" keys — `h`/`l`/
`Tab`/`C-w` are the only mechanism, identical at every width. `\` (toggle rail) and `C-b`
(toggle sidebar) flip *default visibility* at breakpoints that can afford the card; at
narrower breakpoints they focus-summon the drawer (same key, same meaning: "show me it").

## 4.5 Zoom

`Z` toggles the focused card full-bleed inside the card area. Per-card sticky (survives
focus changes and resizes); Esc clears zoom before popping navigation (ladder step 4).

- **Zoomed List** = the triage table: a headed `Table` with real column headers and sort
  arrows — `glyphs · from · to · subject · category(ai) · size · date` (+ account chip in
  unified) — the pressure valve for the 49-col list at the 160 floor, and the only place
  column-header sorting exists (§11).
- **Zoomed Reader** = the full-screen reader (same component, measure re-clamped).
- **Zoomed Rail tab** = full-frame Ask / full entity browser / full contact card.
- Zoomed Sidebar is permitted but pointless; allowed for rule-consistency.

## 4.6 The Esc ladder (one rule, implemented once)

On Esc, exactly the first applicable step fires:

```
1. pending chord/count        → clear it
2. innermost overlay          → close it (pop one from the stack)
3. active stream in focus     → cancel it (search/ask/analyze/find; daemon CANCELLED)
4. zoom on the focused card   → unzoom
5. visual mode / marks        → exit visual, then clear marks
6. active filter on the list  → clear it
7. navigation stack non-root  → pop one breadcrumb level
8. at root                    → nothing (a status hint names `q` / `Ctrl-C Ctrl-C`)
```

`q` performs steps 7–8 only (pop; at root, quit with a 1-line confirm if any send is in
its undo window). **`Ctrl-C Ctrl-C`** (double-tap within 1 s) quits from anywhere,
unbindable. A single `Ctrl-C` behaves as Esc.

---

# 5. Panels, line by line

## 5.1 Header band (row 1)

```
 rmail ▸ work ▸ INBOX      SYNC ✓ 8s ago · IDX ● 97% ⧗12 · AI ⠋3 $0.42/$2 · OUT ◷2 ✗1 · NET ✓ unix      −23 today · Wed Aug 20 14:07
```

- **Left — identity:** `rmail ▸ account ▸ location`; the account segment is tinted with
  that account's accent (`acct1..6`). In unified: `rmail ▸ ∑ unified`.
- **Center — gauges** (each names its expanding verb; `Enter` after focusing it via the
  finder `>` scope, or the leader `Space d …`, opens the detailed Report):
  - `SYNC ✓ 8s ago` — state glyph + freshness; `⠧` braille while a sync streams.
    Tone ladder for all gauges: `? ✓ ↻ ‖ · ! ✗` = unknown/ok/busy/paused/idle/strained/failed.
  - `IDX ● 97% ⧗12` — coverage dot (`ok`/`warn`/`err` by min per-kind coverage) + queue depth.
  - `AI ⠋3 $0.42/$2` — spinner while queue busy, spend vs. the *soft* cap; colored by
    `cap_state` (ok → warn at soft → err at hard, glyph `✗ capped` at hard).
  - `OUT ◷2 ✗1` — scheduled + failed counts (from `WatchOutbox` + `ListOutbox`).
  - `NET ✓ unix` — transport health (`unix`/`tcp`); `✗ retrying ⠙` when reattaching.
- **Right:** `−23 today` session momentum tally (archives+trashes+sends this session;
  ticks on the same frame as the row leaves) + clock.
- Fed by the 5 s heartbeat (`Sync.Status`, `Index.Status`, `Ai.GetUsage`,
  `AiPolicy.GetSpend`) which never increments the inflight counter; `WatchOutbox`
  updates OUT live. Spinners animate only while their operation is live (the tick stream
  exists only then).

## 5.2 Lens strip (row 2, Mail collections) / breadcrumb (elsewhere)

```
 lenses: ▐'a All▌ 'u Unread ~9 · 'r Needs-reply 4 · 'n News 12· · 's Receipts • · '' flip     e archive r reply f filter / search ? help
```

A **lens** is a pinned query over the current mail scope (built-ins compile to
`is:unread`, `ai:needs-reply`, `ai:category:newsletter`, `ai:category:receipt`; users pin
more via `C-s` in search → "pin as lens"). Lenses are *sequential work queues* — the
Superhuman split-inbox model — processed to zero, then on to the next.

- **Every tab renders its own jump chord** (`'a`, `'u`, …; auto-assigned mnemonic letters,
  stable per name) — the chrome teaches the keys. `<`/`>` cycle; `''` flips to the last
  lens; `'?` opens the full switcher (finder, `/` scope).
- **Honest counts** (law 6). There is no count RPC (§19). Rules:
  - The **Unread** lens count is the client unread ledger over loaded rows → rendered `~9`.
  - Any lens visited this session shows its last result count; if `WatchEvents` has since
    dirtied its scope, a trailing `·` marks it stale (`12·`).
  - A lens never visited this session shows `•`, never a number.
  - No background counting by default (unbudgeted searches); `lenses.count_refresh =
    "manual" | "on-idle"` in `tui.toml` may enable one bounded refresh sweep when idle.
- Right side of the strip: the 4–6 key crib for the current context (generated).
- In non-mail collections and in zoomed Reader the strip renders the **breadcrumb**
  instead: `work ▸ INBOX ▸ ⧉ subject ▸ message 2/4` — the navigation stack, always visible.

## 5.3 [1] Sidebar (`Length(22)`)

Content top-to-bottom (one scrollable list; cursor skips headers; `za` folds a section):

```
 ACCOUNTS               ▎● work        ~9      ← ▎ = active, ● = accent dot, ~unread
                          ● personal   ~2
                          ∑ Unified    ~11     ← g u
 MAILBOXES              ▾ INBOX        ~9      ← tree, ▸/▾ folds, unread ledger counts
                          ▸ Clients     3
                            Archive · Sent · Drafts 2 · Junk · Trash
 QUEUES                 ◷ Outbox      2·1✗     ← go   (scheduled · failed)
                        ↩ Follow-ups   3       ← gf
                        ⧗ Waiting-on  5·2!     ← gw   (2 overdue)
                        ✎ Drafts       2       ← g'd… (finder row)
                        ◷ Notifications •      ← gn   (• = unseen alerts)
 VIEWS                  / needs-reply  4       ← saved searches (Enter runs as lens)
                        ◇ travel      12       ← smart folders (◇), count on demand
 TAGS                   ● work 312 · ● finance 41 · …   ← top-N by count, Tag.color dots
 ───────────────
 vol ▂▃▅▂▇▆▃▁▂▄▆█▅▃                            ← 14-day volume sparkline (decoration)
```

- Queue rows render their jump chords (teaching machine), unread/badge counts per the
  ledger rules, and `!` overdue markers.
- `f` filters the tree in place; Enter opens (folder → collection; queue → its
  collection; view → runs as a lens; tag → tag pivot). Per-account braille spinner while
  that account syncs; `!` in `err` on a failed account.
- Folder unread counts follow the ledger rules: `~N` derived, `•` unvisited. Documented
  daemon gap: `FolderStatus` lacks an unread count (§19).

## 5.4 [2] List (`Fill(5)`)

Title is state, k9s-style:

```
─ 2 INBOX </from:acme> ↓date [312/48,213 ·⠿ live] ──
```

folder (or collection name), active **filter chip**, active **sort**, shown/total
(`(partial)` when filtering a partially loaded folder), live-stream marker. Content: a
virtualized `Table` — only the visible slice is built (`offset..offset+height` from
`TableState`); row anatomy in §6. Scroll: 3-row scrolloff; `C-d/C-u` half page with
one-row overlap; nearing the loaded tail requests the next page (header token
`x-rmail-next-page-token`) and shows one ghost row `⠿ loading 500 more…`.

## 5.5 [3] Reader (`Fill(4–5)`)

Master-detail live link (tig): the list cursor re-renders the Reader without moving
focus, via a debounced (40 ms) generation-stamped `Mail.Get` with **cancel-on-scroll** —
holding `j` never queues stale fetches. Shows cached headers instantly; the body region
shows a 3-line shimmer skeleton until `Get` lands. Full spec: §7. For non-message
collections the Reader renders that collection's detail (outbox entry, subscription
detail, rule TOML, audit entry…). Focus it (`l`/Enter) and all reader keys apply;
`Z` zooms it full-bleed.

## 5.6 [4] Rail (`Length(34)`)

Tabbed: `[✦ AI] Thread Ents Contact Why Ask` — always about the cursored row's message,
updating with the cursor (same debounce/cancel as the Reader). When rail is focused,
`[`/`]` cycle tabs; direct jumps: `ge` Ents, `gc` Contact, `w` Why, `A`/`ga` Ask. `\`
toggles; at <L it opens as a drawer.

- **✦ AI** — purely the `<5 ms GetSummary` cache; never a model call by itself:
  status (`✓ deep 13:58` / `⠋ pending` / `not queued — ! analyzes`), priority, category,
  needs-reply; `«tl;dr»`; key points; todos (due/owner); entities; suggested reply
  preview (Enter on its row opens the composer with it); suggested tags with confidence +
  `«rationale»`, `a`/`x` accept/reject the cursored suggestion. `!` re-analyzes (streams
  `AnalyzeMessage` tokens into the tab; labeled `$`); `o` cycles the model for
  re-analysis (arbitration table). `(local)` chip when the local model produced it.
- **Thread** — `GetThread` timeline (who/when/first-line), participants with affinity,
  `«thread summary»` when cached, waiting-on verdict; `j/k` select, Enter jumps.
- **Ents** — entities of the message (amounts, POs, tracking, dates, addresses); Enter =
  "find all mail mentioning this" (`SearchEntities` pivot into the List).
- **Contact** — `GetContactInsight(metrics_only)` card: volume, response symmetry,
  cadence, last exchange; Enter → full contact page (§16.5).
- **Why** — rank explanation for search hits (§9.4).
- **Ask** — the RAG pane (§16.9).

## 5.7 Status bar (1 row, fixed zones)

```
 NORMAL  work/INBOX </f> ↓date  3 marked (1 hidden)  ✗ Move failed: PERMISSION_DENIED — :token list   ⏱ sending to Sara in 8s — u cancels  ⧗1 · ✓ ● ⠋3 $0.42   p▸
```

Zones, left→right: `MODE` (derived) · scope (account/collection + filter + sort echo) ·
marks (with hidden accounting) · **message zone** (the only flexing zone; errors land
here and stick until a keypress, and are *also* appended to the notification feed —
never flash-only) · **undo-send chip** (never evictable, drives from `undo_deadline`) ·
inflight `⧗N` · daemon glyph cluster (mirror when the header is folded) · pending
chord/count.

## 5.8 Keybar (1 row)

The 8 highest-value keys for the focused card/collection, re-rendered on focus change,
generated from the live keymap (drift = failing test). Menus and pickers additionally
display each row's direct key inline (lazygit rule).

---

# 6. Message list row anatomy

## 6.1 Mark + glyph cluster (1 + 4 cells, fixed positions)

Cell 0 is the **mark gutter**: `▌` cursor cap (focused), `✓` marked, `▪` visual-range.
Cells 1–4 are the glyph cluster; **position encodes category** so a missing glyph still
reads (mutt `%Z` lesson). Three icon tiers via the `Icons` struct (unicode default /
nerd opt-in / ascii fallback):

| Cell | Meaning | Unicode | ASCII | NO_COLOR carrier |
|---|---|---|---|---|
| 1 | unread | `●` | `N` | bold row |
| 2 | addressed-to-me / replied / forwarded | `»` `↩` `↪` | `>` `r` `f` | glyph |
| 3 | attachment / scheduled / note | `@` `◷` `¶` | `@` `~` `n` | glyph |
| 4 | AI & safety: injection ⚠ > critical `‼` > high `▲` > needs-reply `↩?`→`?` > pending `⠋` > artifact `✦` | see left | `!` `!` `^` `?` `.` `+` | glyph |

Cell-4 precedence is the listed order. The `✓` mark never collides with glyphs (own cell).

## 6.2 Columns per breakpoint (list inner width *w*; all budgets sum ≤ *w*)

| *w* | Layout (left→right; +1 col spacing between columns) |
|---|---|
| **≥ 96 (XL cozy, 2-line)** | line 1: `mark(1) glyphs(4) from(18) subject(fill) ⧉N(4) chips(≤14) date(7)`; line 2: `└ «tl;dr»`(dim, fill) — from the triage cache only; absent = line 2 shows first text line if cached with the page, else blank collapse to 1-line |
| **72–95 (L compact, 1-line)** | `mark(1) glyphs(4) from(16) subject(fill) ⧉N(3) chips(≤8) date(6)` — no snippet |
| **56–71 (M)** | `mark(1) glyphs(4) from(14) subject(fill) chips→`⟨⟩`glyph date(6)` |
| **40–55 (list at the 160-col frame floor)** | `mark(1) glyphs(3: cells 1,3,4) from(12) subject(fill≈14) date(5)` |
| **< 40** | two-line forced: line 1 `mark glyphs(3) from(fill) date(5)`; line 2 `  subject`(fill) |

Rules: truncation is `unicode-width`-measured with `…` (never byte slicing); From and
Subject end-truncate; addresses/message-ids middle-elide. Tag chips are whole or dropped
to `+N`, never mid-truncated; chip text auto-contrasts on wire `Tag.color`. Density:
`zd` cycles compact / cozy (2-line) / relaxed (cozy + blank row each 5); default compact,
cozy at XL; persisted per collection kind.

## 6.3 Dates (fixed-width right column, `fg_muted`; `fg` on unread rows)

`<24 h → 14:02` · `<7 d → Tue` (7-col tier: `Tue 14:02`) · same year → `Aug 12` · older →
`2024-08`. Scheduled/outbox rows show relative future (`in 2h`) in `scheduled` amber.
Absolute dates always in the Reader.

## 6.4 Search-hit rows

Add, replacing the chip zone: score meter `▮▮▮` (3 cells, `▮`=source agreement:
lexical/dense/entity from `sources[]`), and highlighted matched spans inside
subject/snippet (`SearchHit.snippet.highlights` byte ranges → `match_hl` bg + bold).
`2 similar` chip marks near-duplicate collapse; `⧉N` marks server thread collapse.

## 6.5 Threading & time buckets

- Folder collections are **flat** (the daemon lists messages; client re-threading over
  partial pages would lie). Rows sharing a `thread_id` among loaded rows get a `⧉N` chip.
  `pt` (or Enter on the Reader thread line) pivots to the exact thread via `GetThread`,
  rendered as a collection with tree arms `├╴ ╰╴` **in their own 2-col column** — never
  drawn into the subject string. A `Mail.ListThreads` RPC is a stated gap (§19).
- Search collections thread server-side: `ot` toggles `thread_collapse` on the request.
- **Time buckets**: on date-sorted lists at ≥30 terminal rows, section headers
  `TODAY / YESTERDAY / THIS WEEK / AUGUST / 2025` render as non-addressable rows the
  cursor skips (`fg_faint`, `─` fill).

## 6.6 Selection, cursor, marks

- Cursor row: full-row `bg_selection` bar + `▌` cap when its card is focused;
  `bg_select_blur` without cap when not. Never `REVERSED`.
- `x` toggles mark (`✓` in the gutter); `v` visual range (`▪` on covered rows; motions
  extend; any verb applies to the range and exits). `X` clears all marks.
- **Marks survive filtering and scrolling.** The status zone shows `3 marked (1 hidden)`
  when the filter hides marked rows; a bulk verb over marks that include hidden rows
  asks once: `includes 1 filtered-out message — proceed? y/n`.
- Unread rows bold; read rows in muted lenses (News) render `fg_muted`.

---

# 7. Reader

## 7.1 Measure & scrolling

Body text wraps to a **reading measure**: when `inner_width ≥ 80`,
`measure = min(inner_width − 8, 100)` and the column is centered (margins absorb the
rest); when `inner_width < 80`, `measure = inner_width − 2` (no centering). Net effect:
72–100-col lines wherever the card can afford them, full use of narrow cards, and never
a 130-col line. This
applies to message bodies, the Digest renderer, Ask answers, and the Manual. Pre-wrapped
via `textwrap` into `Vec<Line>` cached on `(message_id, width, fold_state)`; scrollbar,
keys (`j/k`, `C-d/C-u`, `gg/G`), and the status `line x–y of n · 82%` all share the
wrapped-line offset. `f` = find-in-message (`n`/`N` jump); deliberately distinct from `/`
(global search) — *f finds within what you see* (§10).

## 7.2 Header block

Six weeded headers + relationship context; `i` toggles all raw headers inline (scrollable):

```
 From     Amara Okafor <amara@acme.io>        » to you · 14 threads · replies ~2h
 To/Cc    kian@rogon… · finance@acme.io
 Date     Wed Aug 20 2026 14:02 (+2 m)
 Subject  Q3 invoice — net-30 or upfront?
 Thread   ⧉ 4 · you ↔ Amara ↔ finance@ · started Mon      pt opens thread
 Tags     ●work ●finance · ✦?«vendor» (a/x) · ¶ 1 note
```

The relationship hint (threads count, reply latency) comes from the cached contact
insight; absent silently when uncached. Injection-flagged messages insert a full-width
banner directly under the headers: `⚠ prompt-injection suspected (hidden text) — AI
actions withheld · Space a c to review` — driven by `ScanInjection.actions_withheld`.

## 7.3 AI capsule

A 2-line fold under the headers (`\` collapses; full detail in the rail):

```
 ✦ needs reply · ▲ high · deep 13:58                      . actions · ! re-analyze
 «Wants net-30 reissue with PO-88231 today to make the Friday payment run.»
```

Reads the <5 ms cache only. `⠋ pending` while triage hasn't landed; `✗ failed — !
retries` on error; `✦ analyze (!) — $` affordance when not queued. All model text is
`«»`-quoted in `ai` purple (law 9); `(local)` chip on local-model output.

## 7.4 Body rendering rules

- **Daemon-decoded text only** (plain/multipart/QP/base64/RFC 2047 handled server-side).
  format=flowed rewrapped to the measure.
- **Quotes:** leading `>`-runs become a `▎` gutter, depth-colored `quote1..4` cycling;
  quoted text `fg_muted`. Blocks >4 lines fold to `▸ 12 quoted lines — za`; expansion
  remembered per message. `zq` folds/unfolds all quotes. Attribution lines
  ("On …, X wrote:") render `fg_faint` — structure, not content.
- **Signatures:** RFC 3676 `-- ` + heuristics (trailing block with phone/URL patterns);
  rendered `fg_faint`, folded to one rule line by default; `zs` toggles. Legal footers
  (long trailing paragraph heuristic) likewise.
- **HTML mail:** prefer a non-trivial `text/plain` part; else the daemon's extracted
  text with a title chip `html · H opens browser`. **No inline HTML engine** (PRD).
  `H` writes sanitized HTML to a 0600 temp file and opens the browser.
- **Patches:** `+`/`−` lines on subtle bg tints (`diff_add_bg`/`diff_del_bg`), `@@` in
  `info` bold — mail from git users renders like tig.
- **Links:** inline `[n]` markers at occurrence (spans from `ExtractLinks`); a LINKS
  strip at the bottom orders by value score with kind chips
  (unsubscribe/meeting/document/CTA) and `⚠ spoofed-host` on `deceptive` (report, never
  repair). `gl` enters hint mode: numbers highlight, typing one opens — **the URL is
  echoed in the status zone before opening** (phishing defense; deceptive links require
  an extra `y`). `y`+number copies (OSC 52 + arboard; `copied ✓` confirm). OSC 8
  hyperlinks are emitted additionally where the terminal supports them — progressive
  enhancement; the numbered path is the real path.
- **Attachments strip:** one row per attachment (name, size, type). `a` opens the
  attachment browser overlay — verbs per §8.3: Enter open (temp + opener), `s` save
  (streamed `GetAttachment`, progress in the jobs feed), `v` view image
  (`ratatui-image`: kitty/iTerm2/sixel with half-block fallback, dedicated overlay,
  never in-flow), `t` extract tables, `i` extract invoice, `?` ask-attachment ($).
  Results land in Report overlays with per-field provenance (`parsed` plain vs
  `«model»`).
- **Entities:** chips underlined in `entity` color at their spans; Enter-able (pivot).
- **Notes:** `¶ NOTES` block under the headers (markdown, newest first, 6-line
  collapse); `Space n n` add, `Space n e` edit in `$EDITOR` (suspend + full repaint on
  return); `WatchNotes` refreshes concurrent edits live.
- **Thread strip:** earlier thread messages as collapsed one-line headers above the
  body (`▸ Aug 15 you — replied: happy with §4…`); Enter expands in place; `[`/`]` walk
  the thread.

## 7.5 Reading flow

`J`/`K` move to next/prev list row **without leaving the Reader** (auto-advance
backbone). `e`/`d`/`s`/`t` and send act on the current message and **auto-advance**
(direction configurable: `advance = "down" | "up" | "stay"`). Arriving from a search
with `query_id ≠ 0`, opens/dwell/scroll feed `LogFeedback` transparently; the search
footer said so once (`feedback logged`).

---

# 8. The keyboard model

## 8.1 Philosophy

Modal vim: **NORMAL** (cards + collections), **VISUAL** (range), **INSERT** (any text
field), plus overlay contexts that *add* keys (law 4). Counts prefix motions (`5j`,
`3G`, `2C-d`); **digits are always counts** — never panel jumps. Chord families with
instant which-key: `g` go · `p` pivot · `o` order · `y` yank · `z` view/fold · `'`
collection marks · `]`/`[` next/prev-by-kind · `Space` leader (mirrors the command
tree). Every key is a macro for a named verb; `run_verb` is the single dispatch path.
Reserved unbindable: `Esc`, `Ctrl-C`, `:`.

## 8.2 Keymaps

**NORMAL — List card focused** (Reader/Visual inherit → Normal → Global):

| Key | Action | Key | Action |
|---|---|---|---|
| `j/k ↑↓` | cursor (count) | `e` | archive + advance |
| `gg / G` | first / last (count = row) | `d` | trash (undoable, no confirm) |
| `C-d/C-u PgDn/PgUp` | half/full page | `D` | delete permanently (confirm) |
| `Tab / S-Tab` | cycle card focus | `m` / `M` | move / copy (folder picker) |
| `h / l` | focus card left / right | `U` | toggle read |
| `C-w h/j/k/l` | explicit directional focus | `s` | toggle star |
| `Enter` | open (promote focus to Reader) | `r / R / F` | reply / reply-all / forward |
| `Esc` | the ladder (§4.6) | `c` | compose new |
| `q` | pop; at root quit | `t / T` | tag message / thread (palette) |
| `Z` | zoom focused card | `u` | **undo** (incl. undo-send) |
| `x / X` | mark toggle / clear all | `b` | bubble-up: remind/follow-up (NL time + note) |
| `v` | visual range | `f` | fast filter (§10) |
| `< / >` | prev / next lens | `/` | search (§9) |
| `' …` | lens/collection marks; `''` flip | `C-p` | finder (`C-k` = `:` alias) |
| `]u [u ]r [r ]f [f` | next/prev unread / needs-reply / flagged | `:` | command line |
| `o …` | sort chord (§11) | `.` | AI quick menu |
| `p …` | pivot chord (below) | `\` | toggle rail |
| `y …` | yank chord (below) | `C-b` | toggle sidebar |
| `g …` | goto chord (below) | `A` | Ask (rail tab) |
| `z …` | view chord: `za` fold section/quote block · `zq` all quotes · `zs` signature · `zd` density | `w` | why-ranked (search/notification rows) |
| `Space …` | leader (command tree) | `? / K` | help overlay / manual page |

**Pivot chord `p` — relevant-mail one-keys.** Each opens a pre-filled search collection
(the pivot is provenance: title reads `pivot ▸ from:amara@acme.io`; Esc pops back;
pivots compose and the breadcrumb shows the chain):
`pt` thread (exact, `GetThread`) · `ps` same sender address · `pd` same sender domain
(`from:@acme.io`) · `pr` same recipient set · `pc` contact both directions
(`from:X OR to:X`) · `pg` same tag (picker if several) · `pe` same entity (picker over
the message's entities → `SearchEntities`).

**Goto chord `g`:** `gm` mail root · `gu` unified · `go` outbox · `gf` follow-ups ·
`gw` waiting-on · `gn` notifications · `gj` jobs · `gd` insights (digest/analytics
hub) · `gv` subscriptions · `gi` invoices · `gr` automation · `gs` settings · `gh`
manual · `ge` rail Ents · `gc` rail Contact · `ga` rail Ask · `gt` suggest tags (`$`
labeled) · `gl` link hints (Reader) · `g/` grep manual · `g1..g9` account N ·
`gx` index status.

**Yank chord `y`:** `ya` sender address · `ys` subject · `ym` RFC message-id · `yl`
link (hint numbers) · `yq` current query · `yp` path of last-saved attachment. OSC 52 +
arboard; `copied ✓` in the status zone.

**Leader `Space`** mirrors the command tree (labels generated): `a` AI (`aa` analyze!,
`ar` retry failed, `as` status, `ab` budget, `au` audit, `ac` confirm-injection,
`ax` scan) · `t` tags (`tl` list, `tr` rules) · `n` notes (`nn` add, `nt` thread note,
`ne` edit, `nl` list) · `s` search/saved (`sv` save lens, `sl` saved list) · `d` daemon
(`ds` sync now, `dp` pause, `di` index status, `dg` gc, `dv` verify) · `r` rules (`rl`
list, `rn` new-NL, `rb` backtest, `ra` agent) · `x` extract (`xe` events, `xt` tasks,
`xi` invoice, `xl` links, `xd` structured) · `e` export · `c` config/settings ·
`h` help (`hh` help, `hm` manual, `ht` trainer).

**Reader adds** (inherits all list verbs, acting on the open message): `J/K` next/prev
message · `i` toggle headers · `H` open HTML in browser · `gl` link hints · `a`
attachment browser · `[ ]` prev/next in thread · `f` find-in-message (`n/N`) ·
`za/zq/zs` folds.

**INSERT (all text fields):** `Enter` submit · `Esc` cancel (drafts keep text) · `Tab`
complete/next · `↑↓` history or completion list · `C-w`/`C-u` word/line kill ·
bracketed paste = one atomic insert, control chars stripped. Counts/chords disabled.

**VISUAL:** motions extend; `o` swaps ends (vim; documented shadow); any verb applies
to the range and exits.

**Confirm:** `y`/`n`; type-the-name for nuclear ops (delete account, index rebuild,
empty trash). **Menus/pickers:** j/k, Enter, `q`/Esc; every row shows its direct key.

## 8.3 The arbitration table (every context-dependent key, with rationale)

This table is exhaustive by design; anything not listed obeys law 2 (one meaning).

| Key | Global meaning | Context override | Why |
|---|---|---|---|
| `o` | sort chord (list) | rail ✦AI tab: cycle re-analysis model | PRD-canonical; rail has nothing sortable |
| `o` | — | visual mode: swap range ends | vim muscle memory; visual is a mode, not an overlay |
| `s` | star | attachment browser: save; outbox rows: send-now | rows aren't starrable; PRD-canonical outbox key |
| `e` | archive | outbox rows: edit body; invoice rows: export CSV | rows aren't archivable; PRD-canonical |
| `t` | tag palette | outbox rows: reschedule (time picker) | rows aren't taggable; time mnemonic |
| `R` | reply-all | outbox failed rows: retry | rows aren't repliable; PRD-canonical |
| `u` | undo | outbox rows: cancel scheduled | cancel *is* the undo of a scheduled send |
| `a` | attachment browser (Reader) | suggestion rows (rail/chips): accept (`x` reject) | PRD-canonical accept/reject pair; suggestion rows are not messages |
| `x` | mark toggle | suggestion rows: reject | pair with `a` above |
| `!` | — | AI surfaces: force/re-analyze | bang = force, mirrors `:verb!` |
| `w` | why-ranked | waiting-on rows: (none — `w` inert) | why-ranked needs a ranked row |
| `n/N` | find match next/prev (Reader/manual) | — | notes live under `Space n` (PRD `n` re-homed; see §21) |

**PRD conflicts, resolved:** archive = `e` (not `a`); mark = `x`, so **why-ranked = `w`**
(the PRD's `x` collided with reject-tag by its own admission); `/` = search (finder stays
`C-p`); `u` = universal undo; suggested reply opens via **Enter on its row** (rail or
capsule) rather than a global `R`; `zt` (jump next needs-reply) = `]r`.

## 8.4 Reaction to input

Every keypress mutates the model synchronously (pure `update`, no clock); queued events
drain before one frame paints (paste/held key = one frame). Mutations needing the daemon
issue generation-stamped `Cmd`s; **optimistic** flag/tag/move/archive/delete render
immediately (row slides out; lens count and tally tick on the same frame) and reconcile
on `WatchEvents`; refusal = rollback + `err` toast naming the RPC and Status code, with
`r retry` (idempotency keys make retry safe). Debounce: search 25 ms · finder 20 ms ·
reader fetch 40 ms · filter 0 ms (client-side) · resize coalesced.

## 8.5 Rebinding

`keys.toml` hot-reload (1 s poll) with the shadow lint (status line + `:keys check`);
`ConfigService.GetKeymap/SetBinding` wire the `c` rebind flow in help. Enhanced chords
(kitty keyboard protocol: `S-Enter`, `C-S-x`) are bonuses with legacy equivalents; the
protocol flags are pushed at startup and popped on exit *and* panic.

## 8.6 Discoverability (four generated layers + teaching)

1. **Keybar** — 8 contextual hints, per focused card/collection.
2. **Which-key band** — instant on any pending chord; grouped; shadowed entries
   struck-through; overflow renders `+N more (?)`.
3. **`?` help overlay** — mode-aware, searchable, grouped by verb path; Enter runs, `c`
   rebinds, `K` jumps to the manual page.
4. **Palette/command rows** right-align their bound chords — a teaching machine.

**Teaching hints:** after three consecutive slow paths for the same action (e.g. typing
`:archive` thrice), a one-line status hint names the direct key (`tip: e archives —
:set hints off to silence`). Rate-limited to one per action per session.

## 8.7 Autocomplete, everywhere text is typed

One completion popup anatomy everywhere (max 8 rows, opens adjacent to the input,
`Tab` accepts, `↑↓` move, typing filters; matched chars highlighted from positions;
dim right-aligned annotation = kind/description/resolved value):

| Input surface | Completes | Source | Ranking |
|---|---|---|---|
| Search prompt, operator position | operator names (`from:` `tag:` `before:` …) | `query::parse::OPERATORS` | prefix |
| Search prompt, after `from:`/`to:`/`cc:` | contacts | finder `@` scope, inline | frecency |
| Search prompt, after `tag:` / `-tag:` | tags (hierarchy-aware) | `ListTags` cache | count desc |
| Search prompt, after `in:` / `account:` | folders / accounts | sidebar cache | tree order |
| Filter prompt | same as search (client-safe subset; others rejected inline) | same | same |
| `:` command line | verb paths, then flags, then flag values | verb registry | 5-tier (path-prefix > word-start > substring > subsequence > description) |
| Compose To/Cc/Bcc | contacts (fragment + initials: `jsm` → John Smith) | finder `@` scope | frecency |
| Compose Attach / `:export --to` | filesystem paths | local fs | dir-first |
| Tag palette (`t`) | existing tags; hierarchy autocompletes per segment; Enter on a new name = create-then-apply | `ListTags` | fuzzy |
| Folder picker (`m`/`M`) | folder tree | sidebar cache | fuzzy |
| Time inputs (`C-l`, `b`, reschedule) | preset chips + free NL; the **daemon-resolved absolute time echoes live** under the input | `ScheduleSend.send_at_nl` dry resolve / client chrono preview | — |
| Finder | everything, by sigil scope | `Finder.Find` | server score |
| Help `/` filter | action ids + chords + descriptions | live keymap | inclusion |

---

# 9. Search

`/` transforms the List card in place — no modal takeover, the cockpit stays. A prompt
row appears under the list title; hits stream best-first below; the Reader follows the
top hit until you move; `w` flips the rail to Why.

```
─ 2 SEARCH “invoice acme…” [47 hits ⠿ · first in 21 ms] ─────────────────────────
 / invoice acme after:jun_        ⇥ operator · ~ semantic · = exact · C-n compile
──────────────────────────────────────────────────────────────────────────────────
▌ 0.92 ▮▮▮  Amara Okafor   Q3 invoice — net-30 or upfront?    ⧉4        14:02
        └ …reissue the Q3 ⟨invoice⟩ with net-30… PO-88231…    #work #finance
  0.87 ▮▮░  Amara Okafor   Q2 invoice — paid confirmation     2 similar Jul 30
  0.71 ▮░░  AWS Billing    Invoice available: August                    09:52
  47 hits · streamed 180 ms · ranker v12 · feedback logged
```

1. **Incremental:** keystroke → 25 ms debounce → cancel prior stream (generation +
   daemon single-query slot) → new `Search`. First hit target <30 ms; the footer reports
   the actual. **Old hits stay visible, dimmed, until the first new batch** (no strobe).
2. **Operators & sigils:** the full grammar (`from: to: cc: subject: body: has: filename:
   larger: smaller: before: after: on: date: is: tag: note: in: account: thread: ai:
   todo: summary:`, quotes, `-` negation); `~` forces semantic, `=` exact; unknown
   `key:value` degrades to free text, never errors. `Tab` completes operators, then
   values: `from:`/`to:` → frecency contacts (finder `@` scope inline), `tag:` → tags,
   `in:` → folders.
3. **NL queries:** `C-n` compiles via `CompileQuery` (needs `ai.invoke`); the plan
   renders as a confirm strip — raw → compiled DSL, per-operator lines, `«model note»`,
   `cached` badge; `Enter` runs it, `e` edits the DSL. Never silently guessed.
4. **Why-ranked (`w`):** rail shows feature contributions as block meters that **sum
   exactly to the score**, retriever sources, the matched span, and `«claude_reason»`
   when L2-reranked. Identical content to CLI `--explain` (three-parity rule). Explain
   failures latch visibly per hit (`w!`), never silently.
5. **Committing:** `Enter` opens the hit and **pins the result set as the collection**
   (breadcrumb `search ▸ “invoice acme”`), so `J/K` walk hits and every verb works;
   Esc pops back to the folder with the previous cursor intact.
6. **Saved & lenses:** `C-s` names the query → "save search" or "pin as lens".
   Recent searches appear in the sidebar VIEWS section.
7. **Degradation badges** in the prompt row: `semantic off — reduced recall`
   (embeddings unavailable) · `indexing… lexical fallback` (cold index) · all-sources-weak
   → an Enter-able hint row `try ~semantic?`.
8. **Feedback:** impressions/opens/dwell via `LogFeedback` when `query_id ≠ 0`;
   the footer notes `feedback logged` (transparency, once).
9. **Cancellation:** Esc aborts the stream (ladder step 3) and restores the previous
   collection instantly from kept rows.
10. **Thread mode:** `ot` toggles `thread_collapse` server-side; collapsed rows show
    `⧉N`, `za` expands the collapsed members inline (they arrived in
    `thread_collapsed[]`).

---

# 10. Filter vs Search vs Finder — one grammar, three engines

| | **Filter `f`** | **Search `/`** | **Finder `C-p`** |
|---|---|---|---|
| Question | "narrow what I'm looking at" | "find mail anywhere, ranked" | "jump to a *thing* by name" |
| Scope | loaded rows of the focused card | whole corpus (daemon) | messages/folders/contacts/saved/tags/commands |
| Engine | client predicate, zero RPC, <1 frame | `SearchService`, streamed best-first | `FinderService`, snapshot batches |
| Feel | list shrinks per keystroke | hits stream in rank order | batches replace, never strobe |
| Persistence | title chip `</from:acme>` until Esc | pin as lens / save | none (navigation) |
| Honesty | `(partial)` chip on partially loaded folders + `/ searches all` hint | full corpus | `scanned N` counter, `indexing…` badge |

**One predicate grammar everywhere** (mutt's deepest lesson): the filter accepts the
search operators it can evaluate client-side (`from: to: subject: is: has: tag: ai:` +
free text over loaded fields) and **rejects the rest inline** — typing `before:2024` in
the filter renders it red with `use / for that`. `C-Enter` escalates the filter into a
real search verbatim. `f` is card-scoped: list = narrow rows, Reader = find-in-message,
sidebar = filter tree. The finder keeps its sigils (`> # @ / :`), scope cycling
(`C-p`/`M-p`), `Tab` multi-select + `C-a` select-all + `BatchAction`, kind glyphs,
`indexing…`/superseded badges, and empty-query recents ranking.

---

# 11. Sorting

`o` is the order chord; which-key shows the menu; every entry displays its key:
`od` date · `of` from · `os` subject · `oz` size · `op` AI priority · `ou` unread-first ·
`or` relevance (search collections only) · `oo` reverse · `ot` thread-collapse toggle
(search). Pressing the active mode's key again reverses.

Indication: the list title always carries `↓date`/`↑from`; the **zoomed List** (§4.5)
draws real column headers with the arrow on the sorted column — the only header-click/
header-arrow surface. Honesty: folder sorts operate on **loaded rows** — the title
appends `(sorts 1,204 loaded — G loads more)` when the folder is larger; `o!` forces
full pagination first (progress toast; folders ≤ 5 k). Search results honor server-side
sort where the plan allows. Sort persists per collection kind in `tui.toml`
(write-through).

---

# 12. Async, progress & honesty machinery

1. **Never block, never blank.** Stale-while-revalidate: switching folders keeps old
   rows dimmed + `↻` title spinner until page 1 lands, then in-place swap. First-ever
   load: 8 shimmer skeleton rows (`░░░` in `fg_faint`) at plausible widths.
2. **Ambient tier** = header gauges (5 s heartbeat; §5.1). **Detailed tier** = each
   gauge's expanding verb opens a Report: sync per-folder table, index coverage meters
   per kind (`LineGauge` from `IndexKindStatus.coverage`) with lag + quarantine counts,
   AI queue/spend, cache stats.
3. **Jobs feed** (`gj`, Ops): background operations — exports, attachment saves, reindex
   drains, bulk actions — each with a `LineGauge`, cancel key, and an outcome row.
   Missing done-sentinels (`ExportDone`, `IndexProgress.done`) are reported as *cut
   short*, never as success.
4. **Toasts:** bottom-right float, one visible + `+N` badge, queue of 5; Undo > priority
   > newest; a live Undo is never evicted; TTL driven by countdown `Cmd`s (no free
   tick). Every toast is also appended to the notification feed — errors are never
   flash-only.
5. **Undo-send** is not a toast: the status-bar chip `⏱ sending to Sara in 8s — u
   cancels`, driven by `undo_deadline`; `u` = `CancelScheduled` (absent id = most recent
   cancelable) and reopens the composer with draft + cursor intact.
6. **Notification feed** (`gn`): durable, resumable `StreamAlerts since_id` history
   merged with local error/toast history; tier-colored rows with `«reason»`; Enter opens
   the message; `w` explains via `ScoreMessage` (threshold, suppression, would-notify).
7. **Offline (IMAP down, daemon up):** the reserved banner row turns amber —
   `▲ offline since 12:02 — retrying ⠙ 4s · queued: 2 sends · 5 flag changes (all
   durable) · reading, search, tags, notes, local-AI all work`. Queued mutations carry a
   `⇡` glyph until reconciled; late sends get their `sent late` marker.
8. **Live events:** `WatchEvents` resumes from the stored `since_seq` (OUT_OF_RANGE →
   resync from `resume_from` + toast `replayed 14 events`). Events are a dirty flag →
   coalesced reloads. **The cursor never moves because of a network event**; inserted
   rows land in place with a 2 s pulse tint.
9. **Frame discipline:** 4 ms budget; no RPC/parse/alloc storms in `ui()`; caches keyed
   `(content, width)`; synchronized-update (DEC 2026) wraps writes where supported;
   spinner ticks run only while a spinner is visible.

---

# 13. Color system

Semantic tokens only; hex lives in one file; a lint test forbids `Color::` literals
elsewhere **and asserts the contrast floors below** (body ≥ 7:1, muted ≥ 4.5:1, faint ≥
3:1 — computed against the painted `bg`). Truecolor default is Tokyo-Night-anchored (the
PRD's palette family), quantized once at startup to 256 (perceptual nearest, hand-nudged
so muted ≠ faint after quantization). The default theme **respects the terminal bg**
(`Reset`); painted variants exist for `light`.

All ratios below are computed (WCAG relative luminance) against `bg #1a1b26` /
`bg_selection #283457`:

| Token | Role | Truecolor | 256 | on bg / on sel |
|---|---|---|---|---|
| `fg` | body text | `#c0caf5` | 189 | 10.6 / 7.6 |
| `fg_muted` | secondary: tl;dr lines, read rows, dates, quoted text | `#9aa5ce` | 146 | **7.0** / 5.0 |
| `fg_faint` | tertiary: signatures, folds, skeletons, attribution | `#6672a3` | 61 | **3.7** / 2.6* |
| `bg_selection` | cursor row (focused card) | `#283457` | 237 | — |
| `bg_select_blur` | cursor row (unfocused card) | `#22273f` | 236 | — |
| `border` | resting borders, rules | `#3b4261` | 238 | (non-text) |
| `border_focus` / `accent` | the ONE hue meaning "focus/here" | `#7aa2f7` | 111 | 6.8 / 4.9 |
| `match_hl` | search/filter/hint match bg (+bold fg) | `#374f8f` | 60 | fg-on-it **4.9** |
| `unread` | unread glyph (+bold) | `#7aa2f7` | 111 | 6.8 |
| `to_me` | `»` addressed-to-me | `#7dcfff` | 117 | 10.0 |
| `flagged` / `scheduled` | user intent / time-armed | `#e0af68` | 179 | 8.6 |
| `pri_high` / `pri_crit` | AI priority | `#ff9e64` / `#f7768e` | 209 / 210 | 8.4 / 6.5 |
| `ok / warn / err / info` | status + report tones (dedicated tokens) | `#9ece6a #e0af68 #f7768e #7dcfff` | 149 179 210 117 | ≥6.5 |
| `ai` | all model output: `✦`, capsule chrome, `«»` text | `#bb9af7` | 141 | 7.4 |
| `entity` | entity chips/underlines | `#73daca` | 116 | 10.3 |
| `link` | links (+underline) | `#7dcfff` | 117 | 10.0 |
| `quote1..4` | quote-depth gutter cycle | `#7aa2f7 #9ece6a #e0af68 #bb9af7` | 111 149 179 141 | ≥6.8 |
| `acct1..6` | per-account accents | PRD tag palette | nearest | — |
| `diff_add_bg` / `diff_del_bg` | patch tints (~12% toward ok/err) | `#20303b` / `#37222c` | 236 / 235 | — |
| tag chips | wire `Tag.color` (the only wire color); chip text auto-contrasts | — | nearest | — |

\* `fg_faint` on a selection row falls below 3:1 → **rule: within cursor/selection bars,
`fg_faint` promotes to `fg_muted`** (enforced in the row renderer, covered by the lint).

Rules: hue never carries meaning **alone** — every colored state pairs with a glyph or
weight, enforced by the `mono` theme (strips all color; must stay fully legible) and by
`«»` for AI provenance (law 9). Gauges interpolate ok→warn→err stops (btop-style).
`NO_COLOR` / `TERM=dumb`: attributes only — bold unread, reverse selection, underline
links/focus titles, `«»` carries AI, glyphs carry the rest; ASCII icon tier
auto-selected. Built-in themes: `dark` (default), `light`, `mono`, `high-contrast`;
`:set theme` live; persisted to `tui.toml`.

---

# 14. Compose

Full-frame app. Entered via `c` (new), `r`/`R` (reply/reply-all — threading headers
frozen at reply time by `CreateDraft`; a visual selection in the Reader quotes only the
selection), `F` (forward), Enter on a draft row, or Enter on a suggested-reply row
(pre-streamed). The sidebar column persists showing THIS DRAFT facts (reply target,
revision cycle, autosave tick); the rail becomes the **Guardian**.

```
 rmail ▸ work ▸ compose                                     … gauges …                       14:14
 INBOX ▸ reply: Q3 invoice — net-30 or upfront?     C-s send · C-l later · C-t optimal · Esc save+close
╭──────────────┬────────────────────────────────────────────────────────────┬──────────────────╮
│ THIS DRAFT   │ From     Kian Ostad <kian@rogon…>  (work)     C-f identity │ GUARDIAN ⚑       │
│ ↩ to Amara   │ To       Amara Okafor <amara@acme.io>                      │ ✓ attachment     │
│ ⧉ thread kept│ Cc       fin_                                              │   mentioned &    │
│ rev 2/3      │          ┌ finance@acme.io   cc'd on 6 threads ┐           │   attached       │
│ C-o cycles   │          │ fin-ap@acme.io    «AP portal» · 2×  │ ⇥ accept  │ ✓ recipients on  │
│ saved 14:14 ✓│          └ felix.nagel@acme… last Jun 12       ┘           │   thread         │
│              │ Subject  Re: Q3 invoice — net-30 or upfront?               │ ⚠ NOTICE «tone   │
│              │ Attach   @ invoice-q3-rev3.pdf 218K    C-a add · d remove  │  reads terse vs  │
│              │──────────────────────────────────────────────────────────  │  your usual»     │
│              │ Hi Amara,                                                  │ ✓ no secrets     │
│              │ Reissued — attached as rev-3 with net-30 terms…▌           │ ✓ no placeholders│
│              │                                                            │ SEND PLAN        │
│              │ ~ body · C-e $EDITOR · C-g ✦ draft/rewrite                 │ now + undo 10s   │
╰──────────────┴────────────────────────────────────────────────────────────┴──────────────────╯
 INSERT  compose · Cc field · draft saved 3s ago    ⇥/⇤ fields · Enter next · Esc normal
```

- **Fields:** `Tab`/`S-Tab` move; To/Cc/Bcc autocomplete is **frecency-ranked**
  (finder `@` scope inline; fragment + initials matching, `jsm` → John Smith); popup
  below the field, `Tab` accepts. `C-f` cycles From identities (accent chip; signature
  and sent-folder re-derive). `C-a` attach (path prompt, filesystem completion).
- **Body:** honest inline editor for the short reply (multi-line, kill ring, bracketed
  paste = one undo unit); **`C-e` suspends to `$EDITOR`** for anything serious (restore
  + full repaint on return). Never re-implement vim. Autosave via `UpdateDraft`
  debounced 2 s (`draft saved ✓` in the app title); Esc = save + close; nothing is ever
  lost.
- **AI (`C-g`):** menu — draft from stub (`DraftReply` when replying), rewrite verbs
  (shorter/longer/formal/casual/warmer/firmer/mirror/custom → `RewriteDraft`).
  Generated text **streams into the real editable buffer** token-by-token (Esc
  mid-stream keeps text so far); it renders `«ai-tinted»` until first hand-edit. `C-o`
  cycles revisions (`ListDraftRevisions`; rev 0 = your original; hand-edits written
  back before switching). Recipients are derived, never model-chosen. `(local)` chip on
  local-model output.
- **Guardian:** `PreflightCheck` runs on field blur and always before send; findings
  severity-tinted. **BLOCK stops send** (fix it or `:send!`); WARN requires one extra
  Enter; NOTICE lists. Model findings never block (wire contract) and carry `«model»`.
- **Send plan (PRD-canonical):** `C-s` send = schedule now + undo window (status chip
  counts down; `u` cancels and reopens the composer). `C-l` Send Later — preset chips
  (tonight · tomorrow 9:00 · Monday 9:00) + NL input echoing the **daemon-resolved
  absolute time live** (`“fri 4pm” → Fri Aug 22 16:00 PDT`). `C-t` Optimal —
  `SuggestSendTime` with its `«rationale»` shown; Enter accepts. `C-u` cycles the undo
  window (can only lengthen). `C-r` toggles `↩? remind in 3d if no reply`
  (`CreateFollowup`, cancel-on-reply, armed on send).
- Narrow terminals: same single column; Guardian folds to a one-line strip above the
  footer (`GUARDIAN ⚠ 1 NOTICE — C-p details`).

---

# 15. State walkthroughs

Each: what the user SEES → the obvious NEXT action.

## 15.1 First run (no accounts)

Full-frame welcome. `AccountService.List` empty → this screen; daemon connectivity shown.

```
╭──────────────────────────────────────────────────────────────────────────────╮
│    r m a i l — local-first mail, with a daemon brain                         │
│    rmaild ✓ connected · unix ~/.local/state/rmail/rmaild.sock                │
│                                                                              │
│    ▌ a   Add account from just an email address                              │
│          autoconfig probes ISPDB/autodiscover/SRV (model fallback asks       │
│          first) · stores nothing until you confirm the TOML it found        │
│      o   OAuth (Gmail / Office365) — browser handoff, daemon completes      │
│      e   Edit rmail.toml by hand (opens a commented template in $EDITOR)    │
│      t   Take the 2-minute key trainer                                      │
│                                                                              │
│    Sync starts in the background; the UI is usable immediately. AI is OFF   │
│    until you set a budget ($0 is a valid answer).                            │
╰──────────────────────────────────────────────────────────────────────────────╯
```

NEXT: `a` → email → `Autoconfigure` spinner ("asking autoconfig/ISPDB/DNS…") →
discovered servers + source badge + warnings + ready-to-paste TOML (verbatim; nothing
stored until confirmed) → credential step (keychain / command / env / OAuth via
`BeginOAuth`, "waiting for browser consent…" with cancel) → `TestConnection ✓` → budget
prompt → the Mail frame with initial sync running. The status bar pins a hint line
(`j/k move · e archive · ? help`) for the first 20 actions.

## 15.2 The trainer (`t`, or `Space h t` anytime)

A full-frame app owned entirely by the TUI — **no fake mailbox state, no fabricated
messages in the model** (the constitution forbids client-invented mail). The trainer
renders its own practice rows inside its own widget, clearly bannered `TRAINER —
practice rows, not your mail`. Each row names the key that dismisses it (`this row is
archived with e`, `mark three rows with x, then d`, `find the row about invoices with
f inv`); performing the action animates the row out and advances; clearing all rows is
the user's first earned zero state. Ten rows ≈ two minutes: `j/k → Enter → e → d/u →
x x d → f → / → 'u → Z → ?`.

## 15.3 Initial sync in progress

Rows appear as headers land (WatchEvents); skeletons below; honest badges everywhere.

```
 SYNC ⠧ 2.4k/min · IDX ⠋ 3% · AI – (no budget) …
─ 2 INBOX ↓date [1,204 of 48,213 ⠿ syncing] ──────────────────────────────
 ▌● Amara Okafor   Q3 invoice — net-30 or upfront?              14:02
  ● Dana Ruiz      Contract redlines attached                   13:41
  ░░░░░░░░░░░░     ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░                ░░░░
 work: INBOX ▮▮▮▮▮░░░ 1,204/48,213 · Sent queued · Archive queued
 index: extract 3% · lexical 2% · search runs in fallback (badge on)
```

NEXT: start triaging immediately — everything works on what's loaded; `/` shows
`indexing… lexical fallback`.

## 15.4 Steady state — a morning triage session, keystroke by keystroke

`'r` (Needs-reply lens, 4) → `Enter` read → Enter on the suggested-reply row → composer
opens pre-streamed → edit two words → `C-s` (Guardian ✓; chip counts down) →
auto-advance to the next → `e e` archive two FYIs (rows slide out; tally ticks `−3
today`) → `]r` jumps to the Pagerduty needs-reply → `.` quick menu ("draft an ack",
"extract incident id") → `''` back to All → `x x x` on three newsletters → `d` (toast:
`3 → Trash — u undoes`) → `f stripe⏎` filter → `s` star the payout → `Esc` (clears
filter) → `'r` shows the earned zero:

```
─ 2 NEEDS-REPLY [0] ────────────────────────────────────────────
                        ✦  all clear
              Cleared 4 needs-reply in 6 minutes.
      > next: 'n News 12 · gf Follow-ups: 3 due today
```

## 15.5 Offline (IMAP down, daemon up)

Amber banner row: `▲ offline since 12:02 — retrying ⠙ 4s · queued: 2 sends · 5 flags
(all durable) · reading, search, tags, notes, local-AI all work`. Queued rows carry `⇡`.
NEXT: keep working, or `Space d s` to force a retry.

## 15.6 Daemon down

Full-frame screen; the TUI keeps last-known data behind it, marked stale.

```
╭──────────────────────────────────────────────────────────────────────────────╮
│   ✗  rmaild is not answering                                                 │
│      socket   ~/.local/state/rmail/rmaild.sock — connection refused          │
│      last ok  14:31:52 (48 s ago) · retry ⠙ in 4 s (attempt 5)               │
│                                                                              │
│      Your mail is safe: the daemon owns all state; this TUI is a thin        │
│      client. Queued actions replay after reconnect via idempotency keys.     │
│                                                                              │
│      ▸ !   start rmaild now (spawns it, then reattaches automatically)      │
│      ▸ r   retry now      ▸ l   tail daemon log (last 50 lines)             │
│      ▸ y   copy the launchctl start command      ▸ q   quit                 │
│                                                                              │
│      Reconnect resumes WatchEvents from seq 48,112 — no missed events.      │
╰──────────────────────────────────────────────────────────────────────────────╯
```

## 15.7 Auth states at startup

- **`AuthStatus.local_login_required`** → a one-field password screen precedes the
  frame (`LoginPassword`; the bearer is held in memory only).
- **Lockout (`RESOURCE_EXHAUSTED`)** → the same screen with a live countdown
  (`locked — try again in 38 s`) and the retry disabled until it elapses.

## 15.8 Empty / edge states

- **Empty folder:** `∅ nothing here · < > other lenses · C-p jump anywhere`.
- **Empty search:** `0 hits for “foo barred” · ⏎ try ~semantic · e edit query ·
  sources weak: lexical only — semantic index 61% built`.
- **Huge mailbox:** title is the tell — `Archive ↓date [500/812,440 ·⠿]`; `G` pages on;
  filter/sort annotate `(partial)`/`loaded`; a one-time hint names search as the
  full-corpus path.
- **Errors:** failed RPC → status message zone `✗ Move failed: PERMISSION_DENIED (token
  lacks mail.write) — :token list` (sticky until keypress; also in the feed); the
  optimistic change rolls back visibly. Parse errors in prompts render red inline; the
  overlay stays open.

---

# 16. Every remaining surface

All are collections (§3.3) in the same List+Reader cards, with row verbs per the
arbitration table, reachable by `g`-chord, sidebar row, `:` verb, and finder.

1. **Outbox** (`go`) — `ListOutbox` + `WatchOutbox` live: state glyph
   (`◷ scheduled · ⧗ sending · ✓ sent · ✗ failed · · canceled · ? uncertain`), to,
   subject, when (+`optimal ✦` marker), origin (`«ai»` rows always show an undo
   window), attempts, `last_error` verbatim. Verbs: Enter inspect · `e` edit body
   (draft-backed) · `s` send-now · `u` cancel · `R` retry · `t` reschedule (NL picker).
2. **Follow-ups** (`gf`) — armed/fired, remind-at, note-to-self banner on resurface;
   `d` dismiss, `b` new. **Waiting-on** (`gw`) — longest-wait-first (wire order),
   overdue red with age, `ask` column = "the one thing being waited on"; `N` drafts a
   nudge (`DraftNudge` → composer). Sidebar badges `3` / `5·2!`.
3. **Notifications** (`gn`) — §12.6.
4. **Jobs** (`gj`) — §12.3.
5. **Insights** (`gd` hub): **Analytics** — response-time p50/p90 you-vs-them, weekly
   trend sparkline, attention-first group table (bottleneck/stalled chips); `q` NL
   analytics (`AskAnalytics`) — answer renders rows + `«narrative»` **with the sandboxed
   SQL always shown** for checkability. **Contact** (`gc` from any message) — volume,
   symmetry, cadence, decay, topics, `«briefing»`, ≤5 next actions (Enter-able).
   **Subscriptions** (`gv`) — sender, class + source badge (HEADER/HEURISTIC/`«MODEL»`),
   read-rate meter, cadence, signals expandable; `U` shows the unsubscribe **proposal**
   (http/mailto/one-click — rmail never unsubscribes itself; `y` copies, Enter opens
   browser); `classify_unknown` is a labeled `$` action. **Invoices** (`gi`) —
   vendor/number/total/due/status, every cell provenance-tagged (parsed plain vs
   `«model»`); `e` CSV export; Enter opens the source message. **Digest** — markdown
   via the Reader renderer (measure-clamped); every line's `[msg:id]` citations
   Enter-able; `]`/`[` walk sections; `r` regenerate (force, `$`); `cached` badge.
6. **Automation** (`gr`): **Rules** — table + TOML detail; `Space` toggles enabled; `n`
   NL synthesis (instruction → generated TOML + 30-day dry-run hits + stats → confirm
   to store); `B` backtest (per-predicate outcome table, model-call/cache stats,
   `«explanation»` per claude_is decision); corrections recorded from mis-fires.
   **Agent** — dry-run by default (`RunInboxAgent`): action table with `«reason»` and
   outcome; mutating runs gated behind scopes + typed confirm; run history.
   **Hooks** — list + `t` test (exit code, stdout/stderr). **Webhooks** — destinations
   (URLs redacted to authority; reveal explicit), deliveries sub-list with replay.
7. **Settings** (`gs`) — full-frame app; the 14 sections, now with **current values
   inline** wherever a read RPC or local state exists (spend via `GetSpend`, provider
   chain via `GetAiProvider` — configured → override → effective + policy mode, keymap,
   theme/density); config-file-only keys render the exact-TOML `ConfigBlock` (path +
   effect timing + open-to-copy). Keys section = conflict-lint view + `c` rebind.
8. **Tokens / audit** (Settings ▸ Security + `Space a u`) — token table (scopes, last
   used, revoke), mint flow (secret shown once; re-run refused), client-auth setup;
   audit ledger: `QueryAiCalls` paginated (model, pass, tokens, cost, redaction level,
   latency, status) with filter row + `ExportLedger` stream.
9. **Ask (RAG)** — rail Ask tab (`A`/`ga`; zoom or narrow = full-frame). Fixed frame
   order rendered honestly: retrieval trace line (`retrieved 24 · packed 9 · withheld 2
   by policy`) → streamed tokens (64 KiB cap, marked truncation) → citations (`[n]`
   aligned with inline markers; **quotes are verbatim mailbox facts, never `«»`-marked**;
   Enter opens) → the daemon's `grounded`/refusal verdict rendered as the daemon's
   claim. Ask-attachment (`?` in the attachment browser) reuses the surface with
   page/span citations.
10. **Help & manual** — `?` mode-aware searchable overlay (Enter runs, `c` rebinds, `K`
    cross-links); `gh` Manual (full-frame reader, measure-clamped, in-page find, `g/`
    grep-all-pages, jump list `C-o`/`C-i`); trainer via `Space h t`.
11. **Multi-account & unified** — sidebar accounts with accents; `g1..g9` switch; `gu`
    unified (`ListUnified`; rows carry a 1-col account-accent gutter; actions route via
    each row's real account/mailbox ids).
12. **Quick menu (`.`)** — for the cursored message: Summarize (cached, free) · Ask
    (pre-filled) · Suggest reply (`$`) · Suggest tags (`$`) · Extract (events/tasks/
    invoice/links) · Mute-rule proposal (opens `:rule new` pre-filled — honest: there is
    no MuteService, §19).

---

# 17. RPC wiring map (implementor appendix)

Streams marked ⟿; debounce in parentheses; all streams generation-stamped + abort-on-
supersede; daemon CANCELLED on supersession is silence.

| Surface | RPCs |
|---|---|
| List (folder) | `Mail.List` ⟿ (page via header token) · flags/move/copy/delete unary with idempotency keys |
| List (unified) | `Mail.ListUnified` ⟿ |
| Reader | `Mail.Get` (40 ms, cancel-on-scroll) · `GetThread` · `ExtractLinks` (on open) · `GetAttachment` ⟿ (saves) |
| Live spine | `Mail.WatchEvents` ⟿ since_seq resume; OUT_OF_RANGE → resync + toast |
| Search | `Search.Search` ⟿ (25 ms) · `Explain` (on `w`, off-page capable) · `CompileQuery` (`C-n`) · `LogFeedback` · `SearchEntities` (pivots) · `SearchAttachments` |
| Finder | `Finder.Find` ⟿ (20 ms; snapshot batches) · `BatchAction` |
| Lenses/saved | `SavedSearch.*` · `RunSavedSearch` ⟿ · smart folders `ListSmartFolderMembers` ⟿ / `Compile` / `Evaluate` |
| Heartbeat (5 s) | `Sync.Status` · `Index.Status` · `Ai.GetUsage` · `AiPolicy.GetSpend` — never counts as inflight |
| Sync verbs | `SyncFolder` · `Pause/Resume` |
| Index Reports | `Index.Status/Verify/Gc/SetPaused` · `Reindex/Rebuild` ⟿ (done-sentinel honesty) |
| AI panel/rail | `Ai.GetSummary` (<5 ms cache) · `AnalyzeMessage` ⟿ (`!`) · `SuggestReply` · `StreamEnrichments` ⟿ (row `⠋`→`✦` updates) |
| Ask | `Ai.AskMailbox` ⟿ · `Attachment.AskAttachment` ⟿ (fixed frame order: trace → tokens → citations → usage → done) |
| Tags | `Tag.*` · `SuggestTags` ⟿ (`$`) · `ResolveSuggestion` (a/x) |
| Notes | `Note.*` · `WatchNotes` ⟿ (live-tail only; reconnect = re-list) |
| Compose | `Compose.CreateDraft/UpdateDraft(2 s)/…` · `DraftReply` ⟿ · `RewriteDraft` + revisions · `PreflightCheck` (blur + send) |
| Send/outbox | `SendScheduler.ScheduleSend/CancelScheduled/Reschedule/SendNow/RetryFailed` · `ListOutbox` · `WatchOutbox` ⟿ (live-tail; reconnect = re-list) · `SuggestSendTime` |
| Follow-ups | `CreateFollowup/ListFollowups/Dismiss` · `ListWaitingOn` · `DraftNudge` · `TrackFollowup` |
| Automation | `Rule.*` (synthesize/backtest/correct) · `Agent.RunInboxAgent/GetAgentRunLog` · `Hook.*` · `Webhook.*` |
| Insights | `Analytics.GetResponseTimes/AskAnalytics/GenerateDigest/GetContactInsight/ListSubscriptions` · `Attachment.ExtractTables/ExtractInvoice/ExportInvoices` · `Extract.*` · `Link.ExtractLinks` |
| Notifications | `Notification.StreamAlerts` ⟿ since_id · `ScoreMessage` (`w`) |
| Safety | `AiSafety.ScanInjection` (on open of flagged) · `ConfirmInjection` |
| Accounts | `Account.*` · `Autoconfigure` · `BeginOAuth`/`CompleteOAuth` (blocks until consent — run with cancel affordance) · `TestConnection` |
| Auth/admin | `ClientAuth.AuthStatus/LoginPassword/SetupPassword` · `Admin.MintToken/ListTokens/RevokeToken` · `Audit.QueryAiCalls/ExportLedger` ⟿ |
| Export | `Export.Export` ⟿ (ExportDone sentinel; jobs feed) |
| Keymap | `Config.GetKeymap/SetBinding` |

---

# 18. Performance budgets & craft rules

Budgets (PRD + this design): TUI attach < 200 ms (first frame before any RPC returns) ·
first search hit < 30 ms end-to-end · finder first batch < 16 ms · open message < 30 ms ·
AI panel read < 5 ms · frame build+diff < 4 ms typical / 16 ms worst · input-to-paint
< 20 ms.

Craft rules (binding; a condensed checklist for review):

1. Event-driven redraw only (dirty flag; coalesce; tick streams only while animating).
2. One `layout_mode(area)`; behavior and rendering both consult it; unit-tested at every
   width 20..400 and height 10..100.
3. `Layout` destructured with `areas()`; `Fill`+`Min`, no percentage soup.
4. Every style from a semantic token; hex in one file; contrast lint (§13).
5. Selection = background bar, never `REVERSED`; focused ≠ unfocused selection.
6. Rounded light borders; focus by color; closed boxes = transient (§4.1).
7. Icons via the three-table `Icons` struct; meaning never by glyph alone.
8. Truncate with `unicode-width` + `…`; middle-elide ids; never overflow a Rect.
9. Pre-wrap with `textwrap`; cache per `(id, width, folds)`; one scroll offset.
10. `Table` for columns, `List` for 1-D; visible-slice building for huge lists.
11. Bracketed paste always on; paste = one atomic insert, control chars stripped.
12. Kitty keyboard protocol: query, push, pop on exit *and* panic hook.
13. Mouse is garnish: click focus/select, double-click Enter, wheel scrolls pane under
    cursor, capture toggleable; nothing mouse-only.
14. OSC 8 progressive enhancement; numbered links are the real path; URL shown before
    open. OSC 52 + arboard for copy, confirmed in status.
15. Images only in the dedicated viewer via `ratatui-image` with half-block fallback.
16. Never `clear()` per frame; synchronized-update wrap where supported; full repaint
    only on resize and `$EDITOR` return.
17. No RPC, parsing, or allocation storms in `ui()`.

---

# 19. Daemon gaps & proposed RPCs (honest list)

Surfaced in the UI as labeled degradations, never faked. Each is a one-line backend ask:

1. **`FolderStatus.unread`** — folder unread counts are client-derived (`~`, `•`).
2. **`Search.Count(query) -> {count}`** — cheap count for lens tabs; until then, lens
   counts follow §5.2's honesty rules.
3. **`Mail.ListThreads`** — thread-per-row folder view; until then folders are flat with
   `⧉N` chips + `pt` pivot.
4. **Snooze service** — `b` is honestly a follow-up + note (resurfaces via alert +
   Waiting-on); it does not hide the message.
5. **MuteService** — the quick menu's "mute" opens rule synthesis pre-filled instead.
6. **`CommandService.ResolveIntent`** — no NL fallback on `:`; the palette stays
   deterministic; NL lives in search compile and finder.
7. **Screener / split-inbox routing state** — lenses approximate splits client-side;
   a HEY-style screener needs daemon sender-verdict storage.
8. **Redaction preview RPC** and **`:ai policy explain`** — policy shown from
   `GetAiProvider`; per-message redaction preview absent.
9. **`AccountService.Update`** — edits are delete+create; the Settings form says so.
10. **Archive RPC** — archive = `Move` to Archive/Archives/All Mail (heuristic,
    per-account overridable in config).
11. **Server-side folder sort** — folder sorts operate on loaded rows, labeled (§11).
12. **Prompt library / conversation memory / bulk-undo service / ghost-text compose** —
    no backend surface; reserved verbs + manual notes, no fake UI.
13. **Encryption/signing indicators** — glyph position reserved; `Message` carries no
    such fields; ships dead until the daemon speaks it.

---

# 20. Deliberately not included

1. **Inline HTML rendering** — PRD forbids it; `H` browser handoff + daemon text
   extraction is safer and better than a half-engine.
2. **Tabs/workspaces** — the frame + collections + breadcrumb + `''` cover the need;
   tab sprawl is aerc/alot's documented failure mode.
3. **Client-side re-threading of folders** — wrong data structure over partial pages
   (§6.5, §19.3).
4. **A hand-rolled full editor** — `$EDITOR` beats any widget; the inline editor covers
   the two-sentence reply.
5. **In-flow inline images** — they escape the buffer diff and flicker; dedicated
   viewer only.
6. **Redo** — inverse-op redo over a drifting IMAP mailbox lies (§2.2.8).
7. **User-configurable printf-style columns** — three densities + the zoomed table
   cover it; printf-config is write-only.
8. **Mouse-first affordances / hover** — garnish only.
9. **Client-invented mail** (tutorial messages in the mailbox, fake counts, fake
   screener) — the trainer renders its own practice rows in its own widget (§15.2).
10. **A second query dialect** — one operator grammar everywhere (§10).

---

# 21. Migration notes from the current TUI

**Carries over verbatim:** the model/update/view split; command registry + parity +
dispatch; Report/Form/ConfigBlock engines (now rendered inside the Reader card or
overlays); which-key engine; help/manual engines (restyled); theme lint; `terminal_safe`;
stream generations; grpc.rs wiring (extended per §17); history + secret filter;
`keys.toml` machinery.

**Replaced:** the three-screen Screen enum (List/Viewer/Manual/Settings) → frame +
collections + full-frame apps; single overlay slot → stack (max 3); headers-only preview
→ full Reader card with body; `O` outbox overlay → outbox collection; toast queue
gains Completion/Priority producers (wired, not dead); status bar zones per §5.7.

**Deleted concepts:** Screen::Viewer as a separate screen (the Reader card zooms);
`q`-to-quit from list root without confirm-if-undo-pending; the AI panel as a bespoke
column (it is rail tab ✦).

**Re-homed keys** (old → new): `a` archive → `e` · `s` toggle-read → `U` · `f` flag →
`s` · `c` copy-to → `M` · `M` move-to → `m` · `o` open-html → `H` · `x` explain → `w` ·
`O` outbox → `go` · `gs` settings → unchanged · leader groups per §8.2. `keys.toml`
users get the shadow lint + a manual page mapping old→new.

---

# Appendix A — Reference frames

Illustrative at their stated widths; the **normative artifacts are the width tests**
(the view test suite renders every frame at 80/100/120/160/200 × 24/30/50 and asserts
no overflow, budget sums, and drop orders). `⟨…⟩` marks daemon highlight ranges;
`«…»` marks model text (real glyphs, also rendered in `ai` purple).

## A.1 Mail frame — wide (200×50, XL: S│L│R│C, cozy rows)

```
 rmail ▸ work ▸ INBOX                                       SYNC ✓ 8s ago · IDX ● 97% ⧗12 · AI ⠋3 $0.42/$2 · OUT ◷2 ✗1 · NET ✓ unix                                        −23 today · Wed Aug 20 14:07
 lenses: ▐'a All▌ 'u Unread ~9 · 'r Needs-reply 4 · 'n News 12· · 's Receipts •                                                                e archive  r reply  f filter  / search  ␣ leader  ? help
╭─ 1 SIDEBAR ──────────┬─ 2 INBOX ↓date [1,204/48,213 ·⠿ live] ─────────────────────────────────────────┬─ 3 MESSAGE ───────────────────────────────────────────────┬─ 4 CONTEXT ✦ ────────────────────╮
│ ACCOUNTS             │▌ ●»▲@ Amara Okafor       Q3 invoice — net-30 or upfront?        ⧉4      14:02  │ From    Amara Okafor <amara@acme.io>          » to you    │ [✦ AI] Thread Ents Contact ⟨⟩    │
│▎● work          ~9   │       └ «wants revised quote today; asks net-30 terms»    #work #finance       │ To      kian@rogontechnologies.com                        │──────────────────────────────    │
│  ● personal     ~2   │  ✓●@  Dana Ruiz          Contract redlines attached                    13:41  │ Cc      finance@acme.io                                   │ status   ✓ deep 13:58 · ! redo   │
│  ∑ Unified     ~11   │       └ «two blocking edits in §4; rest accepted»          #work              │ Date    Wed Aug 20 14:02 (5 min ago)      i more headers  │ priority ▲ high  ·  reply: yes   │
│                      │  ✓●   GitHub             [rmail] PR #412: fix QRESYNC gap        ⧉9    13:12  │ Subject Q3 invoice — net-30 or upfront?                   │ category work / invoice          │
│ MAILBOXES            │       └ «CI green; maintainer asked for a rebase»          #oss               │ Thread  ⧉ 4 messages · last reply 14:02   pt open thread  │                                  │
│▾ INBOX          ~9   │   »↩  Priya Sharma       Re: onboarding session Thursday         ⧉3    12:55  │ Tags    ●work ●finance      ✦?«vendor»  a apply · x no    │ «TL;DR                           │
│  ▸ Clients       3   │       └ «confirmed 10:00; wants agenda beforehand»         #work              │───────────────────────────────────────────────────────    │ Wants net-30 reissue + PO no.    │
│    Archive           │  ● ◷  Stripe             Your August payout is on its way             12:30  │ ✦ needs reply · ▲ high · deep 13:58    . acts · ! redo    │ today to hit Friday pay run.»    │
│    Sent              │       └ «payout $8,410 lands Aug 22; no action»             #finance          │ «wants revised quote today; asks net-30 terms»            │                                  │
│    Drafts        2   │   ★   Tom Field          Berlin offsite — flights?               ⧉6    11:58  │───────────────────────────────────────────────────────    │ KEY POINTS                       │
│    Junk              │       └ «needs your flight pick by Friday»                  #travel           │                                                           │ «reissue invoice net-30»         │
│    Trash             │  ●    Linear             Weekly digest: 14 issues moved               11:20  │ Hi Kian,                                                  │ «add PO-88231 to line 3»         │
│                      │       └ «3 blockers cleared; release cut Friday»                              │                                                           │ «deadline: today EOD»            │
│ QUEUES               │   ↩   Sara Chen          Re: candidate feedback                  ⧉2    10:44  │ Thanks for the walkthrough yesterday. Two things on       │                                  │
│ ◷ go Outbox   2·1✗   │       └ «agrees with hire; drafting offer terms»           #work              │ the Q3 invoice before we can push it through AP:          │ TODOS                            │
│ ↩ gf Follow-ups  3   │  ● ▲  Pagerduty          [P2] rmaild-sync error budget 82%            10:31  │                                                           │ □ «reissue invoice (due today)»  │
│ ⧗ gw Waiting  5·2!   │       └ «error rate rising since 09:10 deploy»              #work             │  1. Can you reissue with net-30 terms? Our vendor         │ □ «confirm PO on line item»      │
│ ✎ Drafts         2   │       AWS Billing        Invoice available: August                    09:52  │     portal [1] rejects anything shorter this quarter.     │                                  │
│ ◷ gn Notifs      •   │       └ «monthly invoice $1,204.55 attached»                #finance          │  2. The line item for “index tuning” needs a PO           │ ENTITIES                         │
│                      │  ●    The Pragmatic Eng  The AI tooling consolidation                 09:15  │     number — use PO-88231 [2].                            │ $ 12,400.00 USD   # PO-88231     │
│ VIEWS                │       └ «newsletter; 12 min read»                           #news             │                                                           │ ◷ Friday payment run             │
│ / needs-reply    4   │   »   Miguel Ortiz       Intro: Fenwick <> Rogon                 ⧉2    08:40  │ If you can turn this around today we will make the        │                                  │
│ / big-attach     •   │       └ «warm intro; proposes call next week»               #work             │ Friday payment run.                                       │ SUGGESTED REPLY  ✦ drafted       │
│ ◇ travel        12   │   @   DocuSign           Completed: NDA — Fenwick                     08:22  │                                                           │ «Reissued with net-30 and        │
│ ◇ oss-prs        3   │       └ «fully executed copy attached; filed»               #work             │ ▎ On Tue, Aug 19, Kian Ostad wrote:  ▸ 12 quoted — za     │  PO-88231 — attached. Flag…»     │
│                      │       Calendly           New event: design review Fri 14:00            Tue   │                                                           │ ⏎ open in composer               │
│ TAGS                 │  ●    Hacker News Digest Top: SQLite as an application file…           Tue   │ ─ ─ signature (dimmed) — zs ─ ─                           │                                  │
│ ● work         312   │       └ «newsletter; skim-worthy»                           #news             │                                                           │ SUGGESTED TAGS                   │
│ ● finance       41   │   ★   Mom                Sunday dinner?                          ⧉5     Tue   │ Attachments  @2 · a opens strip                           │ ✦ «vendor» 0.91   a yes · x no   │
│ ● travel        17   │       └ «asks if you are coming; bring wine»                #personal         │  [1] invoice-q3-rev2.pdf   214 KB  pdf                    │                                  │
│ ● oss            9   │  · · ·  1,188 more loaded · 48,213 in folder · · ·                            │  [2] po-terms.docx          38 KB  docx                   │                                  │
│ ─────────────────    │                                                                                │                                                           │                                  │
│ vol ▂▃▅▂▇▆▃▁▂▄▆█▅▃   │                                                                                │ Links  gl hints · y+n copies                              │                                  │
│                      │                                                                                │  [1] acme.io/vendor-portal   [2] acme.io/po/88231         │                                  │
╰──────────────────────┴────────────────────────────────────────────────────────────────────────────────┴───────────────────────────────────────────────────────────┴──────────────────────────────────╯
 NORMAL  work/INBOX ↓date  2 marked   ⏱ sending to Sara in 8s — u cancels        ⧗1 · ✓ ● ⠋3 $0.42
 e archive  d trash  r reply  s star  t tag  x mark  p pivot…  o sort…  ⏎ open  ⇥ card  . ai  : cmd  ^P finder
```

## A.2 Mail frame — narrow (100×30, S: List stacked over Reader)

```
 rmail ▸ work ▸ INBOX                                                 ✓8s ●97% ✦⠋3 $0.42 ◷2 ─ 14:07
╭─ 2 INBOX ↓date [1,204/48,213 ⠿] ─────────────────────────────────────────────────────────────────╮
│▌ ●»▲@ Amara Okafor    Q3 invoice — net-30 or upfront?                                      14:02 │
│  ✓●@  Dana Ruiz       Contract redlines attached                                           13:41 │
│  ✓●   GitHub          [rmail] PR #412: fix QRESYNC gap                                     13:12 │
│   »↩  Priya Sharma    Re: onboarding session Thursday                                      12:55 │
│  ● ◷  Stripe          Your August payout is on its way                                     12:30 │
│   ★   Tom Field       Berlin offsite — flights?                                            11:58 │
│  ●    Linear          Weekly digest: 14 issues moved                                       11:20 │
│  ● ▲  Pagerduty       [P2] rmaild-sync error budget 82%                                    10:31 │
├─ 3 MESSAGE ──────────────────────────────────────────────────────────────────────────────────────┤
│ Amara Okafor <amara@acme.io>                                                        » to you    │
│ Q3 invoice — net-30 or upfront?                             Aug 20 14:02 · ⧉4 · ●work ●finance  │
│ ✦ needs reply · ▲ high — «wants net-30 reissue + PO today»                      . acts · \ ai   │
│──────────────────────────────────────────────────────────────────────────────────────────       │
│ Hi Kian,                                                                                        │
│                                                                                                 │
│ Thanks for the walkthrough yesterday. Two things on the Q3 invoice before                       │
│ we can push it through AP:                                                                      │
│                                                                                                 │
│  1. Reissue with net-30 terms — portal [1] rejects shorter.                                     │
│  2. “index tuning” line item needs PO-88231 [2].                                                │
│                                                                                                 │
│ ▎ [12 quoted lines — za]                                                                        │
│ @2 invoice-q3-rev2.pdf · po-terms.docx    a attachments · gl links                              │
╰──────────────────────────────────────────────────────────────────────────────────────────────────╯
 NORMAL  work/INBOX ↓date   ✓ ● ⠋3   e:arc r:rep f:flt /:srch ?:help
 e archive  d trash  r reply  s star  x mark  t tag  ⏎ open  ⇥ card  : cmd
```

## A.3 Zoomed Reader (200×50 → `Z`; measure = 100, centered)

```
 rmail ▸ work ▸ INBOX ▸ ⧉ Q3 invoice — net-30 or upfront? ▸ message 2/4        J/K next/prev · [ ] thread · h back to list · gl links · ? help
╭───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╮
│                          From     Amara Okafor <amara@acme.io>          » to you · 14 threads · replies ~2h                                │
│                          To/Cc    Kian Ostad · finance@acme.io                                                                             │
│                          Date     Wed Aug 20 2026 14:02:11 (+7 m)                        i full headers · H open HTML                      │
│                          Subject  Q3 invoice — net-30 or upfront?                                                                          │
│                          Thread   ⧉ 4 · you ↔ Amara ↔ finance@acme.io · started Mon                pt thread view                          │
│                          Tags     ●work ●finance · ✦?«vendor» (a/x) · ¶ 1 note                                                             │
│                          ─────────────────────────────────────────────────────────────────────────────────────────                        │
│                          ✦ needs reply · ▲ high · deep 13:58                                       \ collapse · ! redo                     │
│                          «TL;DR  Wants net-30 reissue with PO-88231 today to make the Friday payment run.»                                 │
│                          «TODOS  □ reissue invoice (today) · □ confirm PO on line 3»               ⏎ suggested reply                       │
│                          ─────────────────────────────────────────────────────────────────────────────────────────                        │
│                          ▸ Aug 18  Amara — first ask            ▸ Aug 19  you — sent rev-1, terms question                                 │
│                                                                                                                                            │
│                          Hi Kian,                                                                                                          │
│                                                                                                                                            │
│                          Thanks for the walkthrough yesterday. Two things on the Q3 invoice before we can                                  │
│                          push it through AP:                                                                                               │
│                                                                                                                                            │
│                            1. Can you reissue with net-30 terms? Our vendor portal [1] rejects anything                                    │
│                               shorter this quarter.                                                                                        │
│                            2. The line item for “index tuning” needs a PO number — use PO-88231 [2].                                       │
│                                                                                                                                            │
│                          If you can turn this around today we will make the Friday payment run.                                            │
│                                                                                                                                            │
│                          ▸ On Tue, Aug 19, Kian Ostad wrote:   ▎ 12 quoted lines — za                                                      │
│                          ─ ─ signature (dimmed) — zs ─ ─                                                                                   │
│                                                                                                                                            │
│                          ATTACHMENTS  ▌[1] invoice-q3-rev2.pdf 214K pdf   [2] po-terms.docx 38K     a browse                               │
│                          LINKS  [1] acme.io/vendor-portal CTA·0.82   [2] acme.io/po/88231 DOC·0.77  gl hints                               │
╰───────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
 NORMAL  reader · line 1–28 of 41 · 68%     r reply  R all  F fwd  e archive  d trash  u undo  Z unzoom
```

## A.4 Zoomed List — the triage table (`Z` on the list, ≥100 cols)

```
─ 2 INBOX ↓date [1,204/48,213] · triage table · o sorts by column ────────────────────────────────────
  FLAGS  FROM             TO        SUBJECT                          CAT(ai)     SIZE   DATE ▾
▌ ●»▲@  Amara Okafor     you       Q3 invoice — net-30 or upfront?  «invoice»   12K    14:02
  ✓●@   Dana Ruiz        you       Contract redlines attached       «work»      840K   13:41
  ✓●    GitHub           you+2     [rmail] PR #412: fix QRESYNC…    «dev»       22K    13:12
```

*End of specification.*
