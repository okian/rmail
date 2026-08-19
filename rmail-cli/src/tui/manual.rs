//! The built-in manual: a compiled-in hypertext with a deliberately tiny
//! renderer, and generated reference pages read out of the live registries
//! (task 103).
//!
//! # Why the pages are compiled in
//!
//! Every authored page is `include_str!`-ed at build time and every generated
//! page is computed from data already in this process — [`Keymap`],
//! `rmail_core::command`'s verb registry, `parity::Command`. Nothing here
//! reads a file, opens a socket, or needs the daemon. That is not a
//! micro-optimisation: the two moments a person most needs the manual are
//! "the daemon will not start" and "I am on a machine where nothing is
//! installed but this binary", and a manual that fails in exactly those
//! moments is not a manual. It is also what lets the whole reconciliation
//! suite run as ordinary unit tests, with no fixture directory to keep in
//! step.
//!
//! # Why the renderer is this small
//!
//! Five constructs: headings, bullets, fenced code, `[[anchor]]` links and
//! `{{…}}` expansions — the acceptance's list, taken literally. There is no
//! horizontal-rule form: the only separator the manual draws is the one above
//! a generated [`footer`], which is chrome rather than something a page asks
//! for, and a construct no page needs is exactly the half-finished state this
//! project's non-negotiables refuse. There is deliberately **no**
//! inline `` `code` `` or `*emphasis*` — naming a key, a command or a
//! capability inside prose is what the `{{…}}` forms are for, and those are
//! *checked*: [`tests`] fails the build when one of them does not resolve
//! against the live registry it names. An inline-code form would be a second
//! way to write the same thing with none of that checking, so prose that
//! said `` `:tag add` `` would keep rendering long after the verb was
//! renamed. A page written with `{{cmd:tag add}}` cannot.
//!
//! The same argument is why `[[anchor]]` carries no link text: the label is
//! the target page's own [`Page::title`], looked up at render time, so a
//! retitled page retitles every link to it and a link to a page that does
//! not exist is a test failure rather than a dead row.
//!
//! # What is generated, and why none of it is prose
//!
//! [`Generated`] holds the four reference pages — the key reference, the
//! command index, the mode/layer diagram, and the capability list — plus the
//! per-page "reaches" footer, which is derived from the `{{cmd:…}}`
//! expansions the page itself uses. Part V's ground rule is that help stays
//! generated from data, and the reason is visible in this crate's history:
//! task 83 hand-maintained the key reference, and the moment `keys.toml`
//! could rebind anything that table began lying to exactly the users who had
//! customised something.
//!
//! # Shape
//!
//! - [`PAGES`] — the registry: an anchor, a title, and a [`Body`].
//! - [`doc`] — a [`Location`] to a [`Doc`]: wrapped, styled, ratatui-free
//!   lines. Pure, and deliberately uncached — a cached document is one that
//!   goes stale against a `keys.toml` reload, which is the class of bug the
//!   generated pages exist to rule out. The work is a `&'static str` walk and
//!   a `Vec` of wrapped lines, with no I/O and no allocation per *character*,
//!   paid twice per keystroke: once by `model::cursor_span` for the row count,
//!   once by the frame. It is bounded by the page set rather than by anything
//!   a peer controls, which is the property that matters here — but a
//!   [`Location::Grep`] renders *every* page to search it, which task 103
//!   left as the thing to measure once task 104 had written the pages rather
//!   than to reason about. Measured, at 45 pages, unoptimised, in the test
//!   container: 6.3 ms for a whole grep render and 0.13–0.25 ms for one page,
//!   so the worst frame — the hit list, rendered twice — is ~13 ms of debug
//!   build and a small fraction of that optimised. That is inside the frame
//!   budget without a cache, and a cache is what would have to be invalidated
//!   on every `keys.toml` reload, so there is none.
//! - [`grep`] — `(pattern, &Keymap) -> Vec<GrepHit>` across every page. The
//!   durable half of `:helpgrep`: task 90's Report consumes exactly this,
//!   whether or not it keeps [`Location::Grep`]'s page as the presentation.
//! - [`highlight`] — split a rendered document's runs on a search pattern.
//!
//! Nothing in this module knows ratatui exists; `Ink` names *what a run
//! means* and `tui::view` maps that onto a [`crate::tui::theme::Theme`]
//! token, the same separation `overlays.rs` keeps.

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::collections::BTreeSet;

use rmail_core::command::{self, Verb};
use rmail_core::parity::{Command as Capability, Effect};

use crate::keymap::{Action, Keymap, Mode};

/// The column authored prose is wrapped at.
///
/// Fixed rather than the terminal's width, for the reason `man` fixes its
/// own: a paragraph reflowed to 200 columns is unreadable, and a document
/// whose line count changes with the window is a document whose cursor,
/// scroll offset and search-hit line numbers all change with the window too.
/// Fenced code is never wrapped at all — it is verbatim by definition.
pub const WRAP: usize = 78;

/// The most hits [`grep`] collects.
///
/// The same cap the streaming overlays use, for the same reason: a
/// one-character pattern matches most of the manual, and a list nobody can
/// walk is not a better answer than a truncated one that says it was
/// truncated.
pub const MAX_HITS: usize = super::overlays::MAX_ROWS;

/// How far a hit list indents the matched text under its page's name.
const HIT_INDENT: usize = 4;

/// How deep a bullet may nest before further indentation is ignored.
///
/// Two levels is every list this manual has any business drawing; a page
/// that nests deeper is a page that wants to be two pages.
const MAX_BULLET_DEPTH: usize = 2;

// ---------------------------------------------------------------------------
// the registry
// ---------------------------------------------------------------------------

/// Where a page's markdown comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// Prose, compiled in with `include_str!`.
    Authored(&'static str),
    /// Markdown built from live state each time the page is read.
    Generated(Generated),
}

/// A reference page with no prose in it at all.
///
/// Each variant is a *derivation*, not a document: what it prints is whatever
/// the registry it reads says at the moment it is read. See this module's
/// docs on why none of these is authored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generated {
    /// Every binding in force, by layer, plus the actions no key runs.
    Keys,
    /// Every verb the `:` grammar knows, with its arguments.
    Commands,
    /// Each [`Mode`], the layers it falls through to, and what it does with
    /// digits and multi-key chords.
    Modes,
    /// Every capability, and which of them the TUI can reach.
    Capabilities,
}

/// One page of the manual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// The stable name `[[…]]` links and [`Location::Page`] address it by.
    /// Kebab-case, and part of this crate's compatibility surface once a
    /// page has been published — a renamed anchor breaks every link to it,
    /// which is why [`tests`] refuses a dangling one.
    pub anchor: &'static str,
    /// The heading a link to this page renders as.
    pub title: &'static str,
    /// Where its markdown comes from.
    pub body: Body,
    /// The [`Action`] ids and verb paths this page is the documented home
    /// of — the `Action`/[`Verb`] → anchor mapping task 102's `K`-on-a-row
    /// needs, and the thing [`home_of`] looks up.
    ///
    /// Declared rather than derived from the page's own `{{keys:…}}`
    /// markers, because "the first page that happens to mention it" is not
    /// the same question as "where should somebody be sent to read about
    /// it": [`START`] names nearly every action in its own summary, and a
    /// derivation would make the index the home of all of them. A
    /// declaration cannot drift into a lie either — [`tests`] refuses an
    /// entry the page does not actually cite, refuses two pages claiming
    /// one id, and refuses an action or verb no page claims at all, so the
    /// mapping is total in both directions.
    ///
    /// Dots and spaces are the same separator here as everywhere else, so
    /// one entry claims an [`Action::id`] and the [`Verb`] derived from it
    /// at once. `helpgrep` is the one entry that is a verb and no action.
    pub documents: &'static [&'static str],
}

/// Every page, in the order the index lists them.
///
/// Authored prose first, grouped the way [`START`] groups it — getting
/// started, concepts, worked examples, practices, then the two reference
/// pages that are prose — and the four [`Generated`] pages last. The order
/// is the reading order, not an implementation detail: nothing else decides
/// which page [`home_of`] returns when more than one could.
pub const PAGES: &[Page] = &[
    Page {
        anchor: START,
        title: "Start here",
        body: Body::Authored(include_str!("manual/pages/start-here.md")),
        documents: &[],
    },
    Page {
        anchor: "tour",
        title: "A tour of the screen",
        body: Body::Authored(include_str!("manual/pages/tour.md")),
        documents: &[
            "command",
            "cursor.down",
            "cursor.up",
            "cursor.top",
            "cursor.bottom",
            "focus.toggle",
            "focus.folders",
            "focus.messages",
            "open",
            "back",
            "cancel",
            "quit",
            "palette",
            "set",
        ],
    },
    Page {
        anchor: "typing",
        title: "Typing, choosing and confirming",
        body: Body::Authored(include_str!("manual/pages/typing.md")),
        documents: &[
            "prompt.accept",
            "prompt.complete",
            "menu.accept",
            "pick.accept",
            "confirm.accept",
            "input.submit",
            "input.backspace",
        ],
    },
    Page {
        anchor: "daemon",
        title: "The daemon",
        body: Body::Authored(include_str!("manual/pages/daemon.md")),
        documents: &[],
    },
    Page {
        anchor: "offline",
        title: "Working offline",
        body: Body::Authored(include_str!("manual/pages/offline.md")),
        documents: &[],
    },
    Page {
        anchor: "manual",
        title: "Reading the manual",
        body: Body::Authored(include_str!("manual/pages/manual.md")),
        documents: &[
            "help",
            "manual",
            "manual.back",
            "manual.forward",
            "manual.next-match",
            "manual.prev-match",
            "manual.grep",
            "helpgrep",
        ],
    },
    Page {
        anchor: "search-vs-finder",
        title: "Search or finder",
        body: Body::Authored(include_str!("manual/pages/search-vs-finder.md")),
        documents: &["search", "search.explain", "finder"],
    },
    Page {
        anchor: "saved-vs-smart",
        title: "Saved searches and smart folders",
        body: Body::Authored(include_str!("manual/pages/saved-vs-smart.md")),
        documents: &[],
    },
    Page {
        anchor: "archive",
        title: "Archive, move, delete",
        body: Body::Authored(include_str!("manual/pages/archive.md")),
        documents: &[
            "message.archive",
            "message.move",
            "message.copy",
            "message.delete",
        ],
    },
    Page {
        anchor: "bulk",
        title: "Acting on many messages",
        body: Body::Authored(include_str!("manual/pages/bulk.md")),
        documents: &["visual.toggle", "visual.swap-ends"],
    },
    Page {
        anchor: "index",
        title: "The index",
        body: Body::Authored(include_str!("manual/pages/index.md")),
        documents: &[],
    },
    Page {
        anchor: "undo",
        title: "Undo, and what cannot be undone",
        body: Body::Authored(include_str!("manual/pages/undo.md")),
        documents: &["outbox", "outbox.cancel"],
    },
    Page {
        anchor: "grounded",
        title: "Grounded answers",
        body: Body::Authored(include_str!("manual/pages/grounded.md")),
        documents: &["ask"],
    },
    Page {
        anchor: "ai-cost",
        title: "What the AI costs",
        body: Body::Authored(include_str!("manual/pages/ai-cost.md")),
        documents: &["ai.panel", "ai.quick"],
    },
    Page {
        anchor: "privacy",
        title: "Privacy and what leaves the machine",
        body: Body::Authored(include_str!("manual/pages/privacy.md")),
        documents: &["message.open-html"],
    },
    Page {
        anchor: "triage-by-selection",
        title: "Worked example: triage by selection",
        body: Body::Authored(include_str!("manual/pages/triage-by-selection.md")),
        documents: &[],
    },
    Page {
        anchor: "rule-from-mistake",
        title: "Worked example: a rule from a mistake",
        body: Body::Authored(include_str!("manual/pages/rule-from-mistake.md")),
        documents: &[],
    },
    Page {
        anchor: "halve-the-ai-bill",
        title: "Worked example: halve the AI bill",
        body: Body::Authored(include_str!("manual/pages/halve-the-ai-bill.md")),
        documents: &[],
    },
    Page {
        anchor: "add-oauth-account",
        title: "Worked example: add a Gmail account",
        body: Body::Authored(include_str!("manual/pages/add-oauth-account.md")),
        documents: &[],
    },
    Page {
        anchor: "find-the-clause",
        title: "Worked example: find the clause",
        body: Body::Authored(include_str!("manual/pages/find-the-clause.md")),
        documents: &[],
    },
    Page {
        anchor: "digest-to-slack",
        title: "Worked example: a digest into Slack",
        body: Body::Authored(include_str!("manual/pages/digest-to-slack.md")),
        documents: &[],
    },
    Page {
        anchor: "recover-interrupted-rebuild",
        title: "Worked example: recover an interrupted rebuild",
        body: Body::Authored(include_str!("manual/pages/recover-interrupted-rebuild.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-triage",
        title: "Practice: triage in one pass",
        body: Body::Authored(include_str!("manual/pages/practice-triage.md")),
        documents: &["message.toggle-read", "message.toggle-flag"],
    },
    Page {
        anchor: "practice-search",
        title: "Practice: say which kind of search you mean",
        body: Body::Authored(include_str!("manual/pages/practice-search.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-tags",
        title: "Practice: tag for retrieval",
        body: Body::Authored(include_str!("manual/pages/practice-tags.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-notes",
        title: "Practice: write down why",
        body: Body::Authored(include_str!("manual/pages/practice-notes.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-rules",
        title: "Practice: write the rule after the second mistake",
        body: Body::Authored(include_str!("manual/pages/practice-rules.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-budget",
        title: "Practice: set the cap before turning AI on",
        body: Body::Authored(include_str!("manual/pages/practice-budget.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-sending",
        title: "Practice: let the undo window do the checking",
        body: Body::Authored(include_str!("manual/pages/practice-sending.md")),
        documents: &["message.reply", "message.forward"],
    },
    Page {
        anchor: "practice-followups",
        title: "Practice: arm the reminder when you send",
        body: Body::Authored(include_str!("manual/pages/practice-followups.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-index",
        title: "Practice: let the index catch up",
        body: Body::Authored(include_str!("manual/pages/practice-index.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-export",
        title: "Practice: export before you rebuild",
        body: Body::Authored(include_str!("manual/pages/practice-export.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-accounts",
        title: "Practice: one account per trust boundary",
        body: Body::Authored(include_str!("manual/pages/practice-accounts.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-tokens",
        title: "Practice: the narrowest token that works",
        body: Body::Authored(include_str!("manual/pages/practice-tokens.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-webhooks",
        title: "Practice: send the link, not the mail",
        body: Body::Authored(include_str!("manual/pages/practice-webhooks.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-notifications",
        title: "Practice: raise the threshold until it is quiet",
        body: Body::Authored(include_str!("manual/pages/practice-notifications.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-keymap",
        title: "Practice: bind in the layer you stand in",
        body: Body::Authored(include_str!("manual/pages/practice-keymap.md")),
        documents: &[],
    },
    Page {
        anchor: "practice-attachments",
        title: "Practice: ask the attachment",
        body: Body::Authored(include_str!("manual/pages/practice-attachments.md")),
        documents: &[],
    },
    Page {
        anchor: "keys-toml",
        title: "keys.toml",
        body: Body::Authored(include_str!("manual/pages/keys-toml.md")),
        documents: &[],
    },
    Page {
        anchor: "config-file",
        title: "The config file",
        body: Body::Authored(include_str!("manual/pages/config-file.md")),
        documents: &[],
    },
    Page {
        anchor: "troubleshooting",
        title: "Troubleshooting",
        body: Body::Authored(include_str!("manual/pages/troubleshooting.md")),
        documents: &[],
    },
    Page {
        anchor: "keys",
        title: "Key reference",
        body: Body::Generated(Generated::Keys),
        documents: &[],
    },
    Page {
        anchor: "commands",
        title: "Command index",
        body: Body::Generated(Generated::Commands),
        documents: &[],
    },
    Page {
        anchor: "modes",
        title: "Modes and layers",
        body: Body::Generated(Generated::Modes),
        documents: &[],
    },
    Page {
        anchor: "capabilities",
        title: "Capabilities",
        body: Body::Generated(Generated::Capabilities),
        documents: &[],
    },
];

/// The page the manual opens on.
pub const START: &str = "start-here";

/// The page `anchor` names, if the registry has one.
#[must_use]
pub fn page(anchor: &str) -> Option<&'static Page> {
    PAGES.iter().find(|page| page.anchor == anchor)
}

/// The page documenting `id` — an [`Action::id`] or a verb path.
///
/// The `Action`/[`Verb`] → anchor mapping task 104's acceptance asks for, and
/// the lookup behind [`crate::tui::model::open_manual_at`] accepting an
/// action id where a page name would go: `:manual message.archive` and
/// task 102's `K` on a key-reference row are the same question, and neither
/// has an anchor to hand.
///
/// Dots and spaces are the same separator, exactly as
/// [`rmail_core::command`]'s parser treats them, so `message.archive` and
/// `message archive` resolve to the same page. `None` only for a string that
/// is neither an action nor a verb — every real one has a home, which
/// [`tests`]' `every_action_and_verb_has_exactly_one_documenting_page` is
/// what keeps true.
#[must_use]
pub fn home_of(id: &str) -> Option<&'static Page> {
    let wanted = split_path(id);
    if wanted.is_empty() {
        return None;
    }
    PAGES.iter().find(|page| {
        page.documents
            .iter()
            .any(|declared| split_path(declared) == wanted)
    })
}

// ---------------------------------------------------------------------------
// locations
// ---------------------------------------------------------------------------

/// What the manual is showing.
///
/// Two variants rather than one, because a cross-page hit list is not a page:
/// it is not in [`PAGES`], nothing links to it, and it is a function of a
/// pattern the reader typed. Keeping it in the same type is what lets the
/// back/forward stack hold it — walking into a hit, reading the page, and
/// `<c-o>`-ing back to the hit list is the whole reason a jump list exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// A page in [`PAGES`], by anchor.
    Page(String),
    /// `:helpgrep`'s cross-page hits for a pattern.
    Grep(String),
}

impl Location {
    /// The manual's front page.
    #[must_use]
    pub fn start() -> Self {
        Self::Page(START.to_owned())
    }

    /// A short label for the status line and the pane title.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Page(anchor) => page(anchor).map_or_else(
                || format!("{anchor} (no such page)"),
                |page| page.title.to_owned(),
            ),
            Self::Grep(pattern) => grep_title(pattern),
        }
    }
}

// ---------------------------------------------------------------------------
// rendered documents
// ---------------------------------------------------------------------------

/// What a run of text in a rendered page *means*. `tui::view` turns each of
/// these into a theme token; nothing here names a colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// Ordinary prose.
    Body,
    /// A heading.
    Heading,
    /// Secondary text: a code block's gutter, a footer, a hit's line number.
    Muted,
    /// A bullet's marker, a `[[link]]`, a `{{cmd:…}}` expansion.
    Accent,
    /// A `{{keys:…}}` chord.
    Chord,
    /// Verbatim text inside a fenced block.
    Code,
    /// A link or an expansion that did not resolve. Rendered loudly on
    /// purpose: [`tests`] makes this unreachable for the compiled page set,
    /// so seeing one at runtime means a `keys.toml` reload removed something
    /// a page names, and silence would be the wrong answer.
    Broken,
    /// Part of an in-page search match ([`highlight`]).
    Match,
}

/// One styled piece of one rendered line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The text to draw.
    pub text: String,
    /// What it means.
    pub ink: Ink,
    /// The page anchor this run navigates to, when it came from `[[…]]`.
    pub link: Option<&'static str>,
    /// Whether [`wrap`] must keep this run on one line.
    ///
    /// True for everything a marker produced, all three of which can contain
    /// a space: a link's label is the target's title ("Key reference"), a
    /// `{{keys:…}}` expansion is chords joined with " / ", a `{{cmd:…}}` is a
    /// multi-segment verb path. Wrapping those as words would put `:message`
    /// at the end of one line and `archive` at the start of the next, and
    /// would style the space between them as prose — neither of which is the
    /// thing the page said.
    pub atomic: bool,
}

impl Run {
    /// A run of prose, wrappable word by word.
    fn new(text: impl Into<String>, ink: Ink) -> Self {
        Self {
            text: text.into(),
            ink,
            link: None,
            atomic: false,
        }
    }

    /// A run that wraps as one unit — what a marker expands to.
    fn atom(text: impl Into<String>, ink: Ink) -> Self {
        Self {
            atomic: true,
            ..Self::new(text, ink)
        }
    }
}

/// One rendered line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocLine {
    /// Its pieces, left to right.
    pub runs: Vec<Run>,
}

impl DocLine {
    /// The page `<enter>` follows from this row: the first link on it.
    ///
    /// First rather than "the one under a column cursor" because this manual
    /// has no column cursor — a row is the unit of selection, exactly as it
    /// is in every other list this TUI draws. Authored pages therefore put
    /// one link per bullet, which is also how they read.
    #[must_use]
    pub fn link(&self) -> Option<&'static str> {
        self.runs.iter().find_map(|run| run.link)
    }

    /// The row's text with its styling dropped — what search matches against,
    /// so a pattern spanning two runs is still *found* even though
    /// [`highlight`] can only paint it within one.
    #[must_use]
    pub fn text(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }

    fn from_runs(runs: Vec<Run>) -> Self {
        Self { runs }
    }
}

/// A rendered page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    /// What the pane's title says.
    pub title: String,
    /// Its lines, top to bottom.
    pub lines: Vec<DocLine>,
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// Render `at` against the bindings currently in force.
///
/// Total: a [`Location::Page`] naming an anchor the registry does not have
/// renders a page saying so, with a link back to the index, rather than
/// returning `None` for every caller to invent a fallback for. Nothing in
/// the UI can produce one — every navigation target is checked first — but
/// `doc` is also what the tests and a future `:manual <page>` call, and a
/// total function is one fewer error path each of them has to carry.
#[must_use]
pub fn doc(at: &Location, keymap: &Keymap) -> Doc {
    match at {
        Location::Grep(pattern) => grep_doc(pattern, keymap),
        Location::Page(anchor) => match page(anchor) {
            Some(page) => page_doc(page, keymap),
            None => missing_doc(anchor),
        },
    }
}

fn page_doc(page: &'static Page, keymap: &Keymap) -> Doc {
    let blocks = parse_blocks(&source(page, keymap));
    let mut lines = render_blocks(&blocks, keymap);
    lines.extend(footer(&blocks));
    Doc {
        title: page.title.to_owned(),
        lines,
    }
}

fn missing_doc(anchor: &str) -> Doc {
    Doc {
        title: "No such page".to_owned(),
        lines: vec![
            DocLine::from_runs(vec![Run::new(
                format!("This build has no manual page called {anchor:?}."),
                Ink::Broken,
            )]),
            DocLine::default(),
            DocLine::from_runs(vec![link_run(START)]),
        ],
    }
}

/// The markdown for one page: borrowed for authored prose, built for a
/// generated one.
fn source(page: &Page, keymap: &Keymap) -> Cow<'static, str> {
    match page.body {
        Body::Authored(text) => Cow::Borrowed(text),
        Body::Generated(what) => Cow::Owned(generate(what, keymap)),
    }
}

// ---------------------------------------------------------------------------
// block structure
// ---------------------------------------------------------------------------

/// One source-level block. Prose lines are joined into a [`Block::Para`]
/// before wrapping, so an authored page may hard-wrap its own source without
/// that wrapping showing up in the render.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Blank,
    Heading {
        level: usize,
        text: String,
    },
    Bullet {
        depth: usize,
        text: String,
    },
    /// One verbatim line from inside a fence.
    Code(String),
    Para(String),
}

fn parse_blocks(source: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para = String::new();
    let mut fenced = false;

    for line in source.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            // A fence toggles; the marker line itself never renders. Prose
            // interrupted by a fence is flushed first, so `text\n```\ncode`
            // does not glue the code onto the paragraph.
            flush_para(&mut para, &mut blocks);
            fenced = !fenced;
            continue;
        }
        if fenced {
            blocks.push(Block::Code(trimmed.to_owned()));
            continue;
        }
        if trimmed.trim().is_empty() {
            flush_para(&mut para, &mut blocks);
            blocks.push(Block::Blank);
            continue;
        }
        if let Some((level, text)) = heading(trimmed) {
            flush_para(&mut para, &mut blocks);
            blocks.push(Block::Heading { level, text });
            continue;
        }
        if let Some((depth, text)) = bullet(trimmed) {
            flush_para(&mut para, &mut blocks);
            blocks.push(Block::Bullet { depth, text });
            continue;
        }
        // An indented line straight after a bullet continues that bullet
        // rather than starting a paragraph — markdown's lazy continuation,
        // restricted to the indented form.
        //
        // Not a nicety: a bullet long enough to need two source lines is the
        // common case in authored prose, and without this its tail rendered as
        // a *separate, flush-left paragraph* sitting between two bullets —
        // wrapped at column 0 instead of hanging under its own text, which is
        // the opposite of what the hanging indent exists for. Indentation is
        // required (rather than accepting markdown's unindented lazy form) so
        // that a paragraph deliberately following a list is still a paragraph.
        if !trimmed.starts_with(char::is_whitespace) || !continues_bullet(&para, &blocks) {
            // Prose. Joined with a space rather than a newline: the paragraph
            // is about to be re-wrapped at `WRAP`, and keeping the source's
            // own line breaks would wrap it twice.
            if !para.is_empty() {
                para.push(' ');
            }
            para.push_str(trimmed.trim());
            continue;
        }
        if let Some(Block::Bullet { text, .. }) = blocks.last_mut() {
            text.push(' ');
            text.push_str(trimmed.trim());
        }
    }
    flush_para(&mut para, &mut blocks);
    // An unclosed fence is the page author's mistake, not a parse failure —
    // the lines inside it already rendered verbatim, which is what they asked
    // for. `tests`' `every_authored_page_closes_its_fences` is what actually
    // catches it.
    blocks
}

/// Whether an indented line would continue the bullet [`parse_blocks`] last
/// pushed: only when the bullet is genuinely the last thing seen, so nothing
/// is glued back onto a bullet that a paragraph already interrupted.
fn continues_bullet(para: &str, blocks: &[Block]) -> bool {
    para.is_empty() && matches!(blocks.last(), Some(Block::Bullet { .. }))
}

fn flush_para(para: &mut String, blocks: &mut Vec<Block>) {
    if !para.is_empty() {
        blocks.push(Block::Para(std::mem::take(para)));
    }
}

/// `## Title` → `(2, "Title")`, for one to three hashes followed by a space.
fn heading(line: &str) -> Option<(usize, String)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 3 {
        return None;
    }
    let rest = line.get(hashes..)?;
    let text = rest.strip_prefix(' ')?.trim();
    (!text.is_empty()).then(|| (hashes, text.to_owned()))
}

/// `  - text` → `(1, "text")`. Depth is one level per two leading spaces,
/// clamped: see [`MAX_BULLET_DEPTH`].
fn bullet(line: &str) -> Option<(usize, String)> {
    let indent = line.len() - line.trim_start().len();
    let text = line.trim_start().strip_prefix("- ")?.trim();
    (!text.is_empty()).then(|| ((indent / 2).min(MAX_BULLET_DEPTH), text.to_owned()))
}

// ---------------------------------------------------------------------------
// blocks to lines
// ---------------------------------------------------------------------------

fn render_blocks(blocks: &[Block], keymap: &Keymap) -> Vec<DocLine> {
    let mut lines = Vec::new();
    for block in blocks {
        match block {
            Block::Blank => lines.push(DocLine::default()),
            Block::Heading { level, text } => {
                let indent = (level.saturating_sub(1)) * 2;
                let runs = restyle(inline(text, keymap), Ink::Heading);
                lines.extend(wrap(runs, indent, indent, Ink::Heading));
            }
            Block::Bullet { depth, text } => {
                let indent = depth * 2;
                // The trailing space is load-bearing, not decoration:
                // `units_of` groups by whitespace, so it is what closes the
                // marker's unit. A bare "•" would glue itself to the bullet's
                // first word and render "•text". `wrap` then re-inserts the
                // separator it discarded, inked as the block's own gap.
                let mut runs = vec![Run::new("• ", Ink::Accent)];
                runs.extend(inline(text, keymap));
                // Hanging indent: the marker's width, so a wrapped bullet
                // lines up under its own text instead of under the bullet.
                lines.extend(wrap(runs, indent, indent + 2, Ink::Body));
            }
            // Verbatim, never wrapped, with a gutter so a code line is
            // distinguishable from prose without relying on colour — the
            // rule `theme::Theme::mono` exists to keep everything honest
            // about.
            Block::Code(text) => lines.push(DocLine::from_runs(vec![
                Run::new("  │ ", Ink::Muted),
                Run::new(text.clone(), Ink::Code),
            ])),
            Block::Para(text) => lines.extend(wrap(inline(text, keymap), 0, 0, Ink::Body)),
        }
    }
    trim_trailing_blanks(&mut lines);
    lines
}

/// Force one ink over a run sequence, keeping links intact — what makes a
/// heading a heading even when it names a `{{cmd:…}}`.
fn restyle(runs: Vec<Run>, ink: Ink) -> Vec<Run> {
    runs.into_iter()
        .map(|run| Run {
            // A broken expansion keeps its own ink: a heading is not a good
            // enough reason to hide that something in it did not resolve.
            ink: if run.ink == Ink::Broken { run.ink } else { ink },
            ..run
        })
        .collect()
}

fn trim_trailing_blanks(lines: &mut Vec<DocLine>) {
    while lines.last().is_some_and(|line| line.runs.is_empty()) {
        lines.pop();
    }
}

/// Greedily fill lines of at most [`WRAP`] columns from `runs`.
///
/// The unit of wrapping is a word for prose and a whole run for anything a
/// marker produced ([`Run::atomic`]), so a run's styling survives wrapping
/// intact. A unit longer than the available width takes a line of its own
/// rather than being split: the strings long enough to hit that case are
/// chord lists, verb paths and RPC names — exactly the ones a reader wants to
/// read, or copy, whole.
///
/// `gap` inks the spaces this inserts. Passed in rather than assumed to be
/// [`Ink::Body`], because a heading's spaces are part of the heading — a
/// wrapped heading with prose-inked gaps renders as a heading with holes in
/// it under any theme whose heading token sets a background.
fn wrap(runs: Vec<Run>, first_indent: usize, hang_indent: usize, gap: Ink) -> Vec<DocLine> {
    let units = units_of(&runs);
    if units.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<DocLine> = Vec::new();
    let mut current: Vec<Run> = Vec::new();
    let mut indent = first_indent;
    let mut column = indent;

    for unit in units {
        let width: usize = unit.iter().map(|run| run.text.chars().count()).sum();
        // `> indent` rather than `!current.is_empty()`: the first unit on a
        // line goes there however long it is, or an over-long one would loop
        // forever looking for a line it fits on.
        if column > indent && column + 1 + width > WRAP {
            lines.push(indented(indent, std::mem::take(&mut current), gap));
            indent = hang_indent;
            column = indent;
        }
        if column > indent {
            current.push(Run::new(" ", gap));
            column += 1;
        }
        column += width;
        current.extend(unit);
    }
    if !current.is_empty() {
        lines.push(indented(indent, current, gap));
    }
    lines
}

fn indented(indent: usize, runs: Vec<Run>, gap: Ink) -> DocLine {
    if indent == 0 {
        return DocLine::from_runs(runs);
    }
    let mut all = vec![Run::new(" ".repeat(indent), gap)];
    all.extend(runs);
    DocLine::from_runs(all)
}

/// One unit [`wrap`] fills lines with: a maximal whitespace-free stretch,
/// which may span several styled runs.
///
/// Several, because the source glues punctuation to markers — `({{keys:open}})`
/// is how a page names a key mid-sentence — and splitting purely on runs would
/// put `(`, the chord and `)` in three units with `wrap`'s own spaces between
/// them, rendering `( <enter> )`. Grouping by *whitespace in the source* is
/// what keeps the sentence looking like the sentence, and it makes the line
/// break fall outside the parenthesis rather than inside it.
type Unit = Vec<Run>;

/// Split `runs` into [`Unit`]s: whitespace in the source separates them,
/// everything else glues.
fn units_of(runs: &[Run]) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    // Whether the unit being built can still take a piece glued onto it —
    // false at the start and immediately after whitespace.
    let mut open = false;
    for run in runs {
        if run.atomic {
            // A marker that expanded to nothing but spaces has no business
            // splitting the words around it.
            if !run.text.trim().is_empty() {
                push_piece(&mut units, &mut open, run.clone());
            }
            continue;
        }
        if run.text.starts_with(char::is_whitespace) {
            open = false;
        }
        let mut words = run.text.split_whitespace().peekable();
        while let Some(word) = words.next() {
            push_piece(
                &mut units,
                &mut open,
                Run {
                    text: word.to_owned(),
                    ink: run.ink,
                    link: run.link,
                    atomic: false,
                },
            );
            if words.peek().is_some() {
                open = false;
            }
        }
        if run.text.ends_with(char::is_whitespace) {
            open = false;
        }
    }
    units
}

fn push_piece(units: &mut Vec<Unit>, open: &mut bool, piece: Run) {
    match units.last_mut() {
        Some(last) if *open => last.push(piece),
        _ => units.push(vec![piece]),
    }
    *open = true;
}

// ---------------------------------------------------------------------------
// inline: links and expansions
// ---------------------------------------------------------------------------

/// Resolve `[[…]]` links and `{{…}}` expansions in one line of prose.
fn inline(text: &str, keymap: &Keymap) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut plain = String::new();
    let mut rest = text;

    while !rest.is_empty() {
        let link = rest.find("[[");
        let expansion = rest.find("{{");
        let Some(at) = min_some(link, expansion) else {
            break;
        };
        let (open, close): (&str, &str) = if Some(at) == link {
            ("[[", "]]")
        } else {
            ("{{", "}}")
        };
        let after = at + open.len();
        // An opener with no closer is literal text, not an error: a page
        // discussing the syntax has to be able to write one.
        let Some(end) = rest.get(after..).and_then(|tail| tail.find(close)) else {
            break;
        };
        plain.push_str(rest.get(..at).unwrap_or_default());
        push_plain(&mut runs, &mut plain);
        let inner = rest.get(after..after + end).unwrap_or_default().trim();
        runs.push(if open == "[[" {
            link_run(inner)
        } else {
            expand(inner, keymap)
        });
        rest = rest.get(after + end + close.len()..).unwrap_or_default();
    }

    plain.push_str(rest);
    push_plain(&mut runs, &mut plain);
    runs
}

fn min_some(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (found, None) | (None, found) => found,
    }
}

fn push_plain(runs: &mut Vec<Run>, plain: &mut String) {
    if !plain.is_empty() {
        runs.push(Run::new(std::mem::take(plain), Ink::Body));
    }
}

/// A `[[anchor]]` link, labelled with the target page's own title.
fn link_run(anchor: &str) -> Run {
    match page(anchor) {
        Some(page) => Run {
            link: Some(page.anchor),
            ..Run::atom(page.title, Ink::Accent)
        },
        None => Run::atom(format!("[[{anchor}]]"), Ink::Broken),
    }
}

/// Resolve one `{{kind:argument}}`.
fn expand(inner: &str, keymap: &Keymap) -> Run {
    let Some((kind, argument)) = inner.split_once(':') else {
        return Run::atom(format!("{{{{{inner}}}}}"), Ink::Broken);
    };
    let argument = argument.trim();
    match kind.trim() {
        "keys" => expand_keys(argument, keymap),
        "cmd" => expand_cmd(argument),
        "capability" => expand_capability(argument),
        _ => Run::atom(format!("{{{{{inner}}}}}"), Ink::Broken),
    }
}

/// `{{keys:message.archive}}` → every chord that runs it, in any layer.
///
/// Every layer rather than one, because the honest answer to "what key does
/// this" depends on where you are standing, and a page naming a mode as well
/// would be a page to keep in step by hand. An action nothing binds says so
/// — that is a fact about the keymap, not a broken expansion, and the
/// generated [`Generated::Keys`] page lists the same set under its own
/// heading.
fn expand_keys(id: &str, keymap: &Keymap) -> Run {
    let Some(action) = Action::from_id(id) else {
        return Run::atom(format!("{{{{keys:{id}}}}}"), Ink::Broken);
    };
    let chords = chords_of(action, keymap);
    if chords.is_empty() {
        return Run::atom("unbound", Ink::Muted);
    }
    Run::atom(chords.join(" / "), Ink::Chord)
}

/// Every distinct chord that runs `action`, across [`Mode::Global`] and every
/// configurable layer, in layer order.
fn chords_of(action: Action, keymap: &Keymap) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for mode in layers() {
        for (chord, bound) in keymap.layer(mode) {
            let spelling = chord.to_string();
            if bound == action && !found.contains(&spelling) {
                found.push(spelling);
            }
        }
    }
    found
}

/// `{{cmd:message archive}}` → `:message archive`, checked against the
/// registry.
fn expand_cmd(path: &str) -> Run {
    let segments = split_path(path);
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    match command::verb_at(&refs) {
        Some(verb) => Run::atom(format!(":{}", verb.canonical()), Ink::Accent),
        None => Run::atom(format!("{{{{cmd:{path}}}}}"), Ink::Broken),
    }
}

/// `{{capability:MailSetFlags}}` → the RPC it is, checked against
/// [`Capability::ALL`].
fn expand_capability(name: &str) -> Run {
    match Capability::ALL.iter().find(|c| c.name() == name) {
        Some(capability) => Run::atom(
            format!("{}.{}", short_service(*capability), capability.method()),
            Ink::Muted,
        ),
        None => Run::atom(format!("{{{{capability:{name}}}}}"), Ink::Broken),
    }
}

/// `rmail.v1.MailService` → `MailService`. The package prefix is the same on
/// every row, so printing it would cost a fifth of the column and say
/// nothing.
fn short_service(capability: Capability) -> &'static str {
    let service = capability.service();
    service.rsplit('.').next().unwrap_or(service)
}

/// Dots and spaces are the same separator, exactly as
/// `rmail_core::command`'s own parser treats them.
fn split_path(text: &str) -> Vec<String> {
    text.split(['.', ' '])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------------------
// the capability footer
// ---------------------------------------------------------------------------

/// The "reaches" footer for a page: every capability behind the verbs the
/// page named with `{{cmd:…}}`.
///
/// Derived from the page's own text rather than declared alongside it, so a
/// page cannot claim an RPC it never mentions or quietly stop mentioning one
/// it claims. Pages that name no commands get no footer at all.
///
/// Reads the parsed blocks rather than the raw markdown so that a `{{cmd:…}}`
/// shown *inside a fence* — a page documenting the syntax, like [`PAGES`]'
/// own `manual` page — does not also count as a command that page reaches. A
/// fence is verbatim everywhere else in the renderer; it has to be verbatim
/// here too or the two disagree about what the page said.
fn footer(blocks: &[Block]) -> Vec<DocLine> {
    let mut capabilities: Vec<Capability> = Vec::new();
    for verb in cited_verbs(blocks) {
        if let Some(capability) = verb.capability {
            if !capabilities.contains(&capability) {
                capabilities.push(capability);
            }
        }
    }
    if capabilities.is_empty() {
        return Vec::new();
    }
    capabilities.sort_unstable_by_key(|c| c.rpc());

    let mut lines = vec![
        DocLine::default(),
        DocLine::from_runs(vec![Run::new("─".repeat(WRAP), Ink::Muted)]),
        DocLine::from_runs(vec![Run::new("What this page reaches", Ink::Heading)]),
        DocLine::default(),
    ];
    for capability in capabilities {
        let runs = vec![
            Run::new("• ", Ink::Accent),
            Run::new(
                format!(
                    "{}.{} ({}) — {}",
                    short_service(capability),
                    capability.method(),
                    effect(capability.effect()),
                    capability.summary()
                ),
                Ink::Muted,
            ),
        ];
        lines.extend(wrap(runs, 0, 2, Ink::Muted));
    }
    lines
}

/// Every verb a page names with `{{cmd:…}}` outside a fence, in the order it
/// names them.
fn cited_verbs(blocks: &[Block]) -> Vec<&'static Verb> {
    let mut verbs: Vec<&'static Verb> = Vec::new();
    for block in blocks {
        let text = match block {
            Block::Heading { text, .. } | Block::Bullet { text, .. } | Block::Para(text) => text,
            Block::Code(_) | Block::Blank => continue,
        };
        let mut rest = text.as_str();
        while let Some(at) = rest.find("{{cmd:") {
            let after = at + "{{cmd:".len();
            let Some(end) = rest.get(after..).and_then(|tail| tail.find("}}")) else {
                break;
            };
            let path = rest.get(after..after + end).unwrap_or_default().trim();
            let segments = split_path(path);
            let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
            if let Some(verb) = command::verb_at(&refs) {
                if !verbs.iter().any(|known| known.path == verb.path) {
                    verbs.push(verb);
                }
            }
            rest = rest.get(after + end + 2..).unwrap_or_default();
        }
    }
    verbs
}

const fn effect(effect: Effect) -> &'static str {
    match effect {
        Effect::Read => "read",
        Effect::Mutate => "mutates",
    }
}

// ---------------------------------------------------------------------------
// generated pages
// ---------------------------------------------------------------------------

/// [`Mode::Global`] first, then every configurable layer.
///
/// Derived from [`Mode::CONFIGURABLE`] rather than restated, the same way
/// `keymap::tests`' `defaults_are_all_installable` derives its own list: a
/// hand-written one silently under-counts when a mode is added, and here
/// that would mean an action bound only in the new layer disappearing from
/// the key reference *and* from the unbound list — documented nowhere, with
/// no test failing.
fn layers() -> Vec<Mode> {
    std::iter::once(Mode::Global)
        .chain(Mode::CONFIGURABLE.iter().copied())
        .collect()
}

fn generate(what: Generated, keymap: &Keymap) -> String {
    match what {
        Generated::Keys => generate_keys(keymap),
        Generated::Commands => generate_commands(),
        Generated::Modes => generate_modes(),
        Generated::Capabilities => generate_capabilities(),
    }
}

fn generate_keys(keymap: &Keymap) -> String {
    let mut out = String::from(
        "# Key reference\n\n\
         Every binding in force right now, read out of the keymap this session \
         loaded — edit keys.toml and this page changes within a second, with no \
         restart. A binding is listed under the layer that declares it; a mode \
         also inherits the layers below it, which [[modes]] sets out.\n\n\
         The global layer is not rebindable: Esc and Ctrl-C are the way out of \
         every mode, and a config file that could take them away could lock \
         someone into a modal screen.\n",
    );

    let mut bound: BTreeSet<Action> = BTreeSet::new();
    for mode in layers() {
        let rows: Vec<(String, Action)> = keymap
            .layer(mode)
            .map(|(chord, action)| (chord.to_string(), action))
            .collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {}\n\n```\n", mode.id()));
        for (chord, action) in rows {
            bound.insert(action);
            out.push_str(&format!(
                "{chord:<10} {:<22} {}\n",
                action.id(),
                action.describe()
            ));
        }
        out.push_str("```\n");
    }

    out.push_str(
        "\n## Actions no key runs\n\nReachable by name — the command line, the \
         palette, or a binding of your own — but bound to nothing today.\n\n```\n",
    );
    let unbound: Vec<&Action> = Action::ALL
        .iter()
        .filter(|action| !bound.contains(action))
        .collect();
    if unbound.is_empty() {
        out.push_str("(none — every action has a key)\n");
    } else {
        for action in unbound {
            out.push_str(&format!("{:<22} {}\n", action.id(), action.describe()));
        }
    }
    out.push_str("```\n");
    out
}

fn generate_commands() -> String {
    let mut out = String::from(
        "# Command index\n\n\
         Every verb the : grammar knows. Dots and spaces are the same \
         separator, so message.archive and message archive are one verb — and \
         every action id is a verb without anything being registered for it, \
         which is why this list and [[keys]] name the same things.\n\n\
         A leading range applies the verb to more than the cursor: '<,'> for \
         the visual selection, % for everything listed, or a bare count for \
         that many rows down. A trailing ! skips the confirmation, and only \
         that.\n\n```\n",
    );
    // `children_of(&[])` is every verb: each one's path is strictly longer
    // than the empty prefix and starts with it. There is no verb *at* the
    // empty path — a bare `:` is the command line's own prompt, not a verb —
    // so this is the whole registry with nothing left out.
    let mut rows: Vec<(String, String)> = command::children_of(&[])
        .into_iter()
        .map(|verb| (signature(verb), verb.describe()))
        .collect();
    rows.sort();
    for (signature, describe) in rows {
        out.push_str(&format!("{signature:<34} {describe}\n"));
    }
    out.push_str("```\n");
    out
}

/// A verb as it is typed: its path, then its positionals, then its flags.
fn signature(verb: &Verb) -> String {
    let mut out = verb.canonical();
    for positional in verb.positionals {
        if positional.required {
            out.push_str(&format!(" <{}>", positional.name));
        } else {
            out.push_str(&format!(" [{}]", positional.name));
        }
    }
    for flag in verb.flags {
        if flag.takes_value {
            out.push_str(&format!(" [--{} <value>]", flag.name));
        } else {
            out.push_str(&format!(" [--{}]", flag.name));
        }
    }
    out
}

fn generate_modes() -> String {
    let mut out = String::from(
        "# Modes and layers\n\n\
         Which keys do what depends on what is on screen, and the mode is \
         derived from that — never stored — so it cannot disagree with what \
         you are looking at. A lookup walks the mode's layers nearest first \
         and stops at the first layer that binds the chord.\n\n\
         The overlay layers stop at global rather than falling through to \
         normal, which is what keeps a key from reaching the message list \
         through a modal that is covering it.\n\n\
         Counts and multi-key chords are off in the layers where keys are \
         text: holding the first key of a chord back inside a text field is \
         indistinguishable from dropping it.\n\n```\n",
    );
    out.push_str(&format!(
        "{:<10} {:<34} {:<8} {}\n",
        "mode", "falls through to", "counts", "chords"
    ));
    for mode in layers() {
        let chain: Vec<&str> = mode.chain().iter().map(|layer| layer.id()).collect();
        out.push_str(&format!(
            "{:<10} {:<34} {:<8} {}\n",
            mode.id(),
            chain.join(" → "),
            yes_no(mode.takes_counts()),
            yes_no(mode.allows_chords()),
        ));
    }
    out.push_str("```\n\nOnly these layers may be named in keys.toml:\n\n```\n");
    for mode in Mode::CONFIGURABLE {
        out.push_str(&format!("{}\n", mode.id()));
    }
    out.push_str("```\n");
    out
}

const fn yes_no(yes: bool) -> &'static str {
    if yes {
        "yes"
    } else {
        "no"
    }
}

fn generate_capabilities() -> String {
    let mut out = String::from(
        "# Capabilities\n\n\
         One row per RPC the daemon serves. This is the whole API surface: if \
         the CLI or this TUI can do it, one of these rows is what it calls, \
         and every row is reachable over gRPC and to an AI agent over MCP.\n\n\
         read means calling it observes state and changes nothing an observer \
         outside the process could see; mutates means it does — including \
         spending money at a model provider, or logging in against somebody \
         else's IMAP server.\n\n## Reachable from this TUI\n\n```\n",
    );
    let mut reachable = 0_usize;
    for capability in Capability::ALL {
        if capability.actions().is_empty() {
            continue;
        }
        reachable += 1;
        let actions: Vec<&str> = capability.actions().iter().map(|a| a.id()).collect();
        out.push_str(&format!(
            "{:<34} {:<8} {}\n",
            format!("{}.{}", short_service(*capability), capability.method()),
            effect(capability.effect()),
            actions.join(", ")
        ));
        out.push_str(&format!("{:<44} {}\n", "", capability.summary()));
    }
    out.push_str(&format!(
        "```\n\n{reachable} of {} capabilities have a key or a command in this \
         TUI. The rest are reachable over gRPC, over MCP, or from the mail \
         command line; a capability with no human surface is not a gap to \
         close, it is an RPC no human types.\n\n## Every capability\n\n```\n",
        Capability::ALL.len()
    ));
    for capability in Capability::ALL {
        out.push_str(&format!(
            "{:<34} {:<8} {}\n",
            format!("{}.{}", short_service(*capability), capability.method()),
            effect(capability.effect()),
            capability.name()
        ));
    }
    out.push_str("```\n");
    out
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/// One `:helpgrep` hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepHit {
    /// The page it is on.
    pub anchor: &'static str,
    /// That page's title.
    pub title: &'static str,
    /// Which rendered line matched, 1-based, and a *rendered* line rather than
    /// a source line — the number the page itself would show.
    ///
    /// Displayed, not navigated to: opening a hit lands on the first match on
    /// that page rather than this one, because a link carries an anchor and
    /// nothing else. Deliberate rather than missing — the pattern arrives
    /// highlighted and `manual.next-match` steps — but it does mean the
    /// fifth hit on a page and the first open the same view.
    pub line: usize,
    /// The matching line, with its styling dropped.
    pub text: String,
}

/// Every line of every page containing `pattern`, case-insensitively.
///
/// An empty pattern finds nothing rather than everything, the same rule the
/// search overlay follows: "match all of it" is not a search anybody asked
/// for, and it is the state the box is in before a key is pressed.
///
/// Pure, and the only thing task 90's Report would need to consume — see this
/// module's docs.
#[must_use]
pub fn grep(pattern: &str, keymap: &Keymap) -> Vec<GrepHit> {
    let needle = pattern.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for page in PAGES {
        for (idx, line) in page_doc(page, keymap).lines.iter().enumerate() {
            if hits.len() >= MAX_HITS {
                return hits;
            }
            let text = line.text();
            if text.to_lowercase().contains(&needle) {
                hits.push(GrepHit {
                    anchor: page.anchor,
                    title: page.title,
                    line: idx + 1,
                    text: text.trim().to_owned(),
                });
            }
        }
    }
    hits
}

fn grep_doc(pattern: &str, keymap: &Keymap) -> Doc {
    let hits = grep(pattern, keymap);
    let mut lines = vec![
        DocLine::from_runs(vec![Run::new(
            format!("Manual pages matching {pattern:?}"),
            Ink::Heading,
        )]),
        DocLine::default(),
    ];
    if hits.is_empty() {
        lines.push(DocLine::from_runs(vec![Run::new(
            "No page mentions it.",
            Ink::Body,
        )]));
    } else {
        let capped = hits.len() >= MAX_HITS;
        lines.push(DocLine::from_runs(vec![Run::new(
            if capped {
                format!("The first {} matching lines. Enter opens one.", hits.len())
            } else {
                format!("{} matching line(s). Enter opens one.", hits.len())
            },
            Ink::Muted,
        )]));
        lines.push(DocLine::default());
    }
    for hit in hits {
        // Built as lines rather than as markdown run back through the parser:
        // a matched line that happens to start with `- ` or contain `[[…]]`
        // is *already rendered text*, and re-parsing it would turn a page's
        // own words into a bullet or a link nobody wrote.
        lines.push(DocLine::from_runs(vec![
            Run {
                link: Some(hit.anchor),
                ..Run::atom(hit.title, Ink::Accent)
            },
            Run::new(format!(":{}", hit.line), Ink::Muted),
        ]));
        lines.push(DocLine::from_runs(vec![
            Run::new(" ".repeat(HIT_INDENT), Ink::Body),
            Run::new(
                super::overlays::truncate_chars(&hit.text, WRAP.saturating_sub(HIT_INDENT)),
                Ink::Body,
            ),
        ]));
    }
    lines.push(DocLine::default());
    lines.push(DocLine::from_runs(vec![link_run(START)]));
    Doc {
        title: grep_title(pattern),
        lines,
    }
}

/// What a hit list calls itself, in the one place both its pane title and its
/// [`Location::label`] read it from.
fn grep_title(pattern: &str) -> String {
    format!("helpgrep {pattern:?}")
}

/// Which lines of `doc` contain `pattern`, case-insensitively.
///
/// Matched against each line's whole text (see [`DocLine::text`]), so a hit
/// that straddles two runs is still located even though [`highlight`] can
/// only paint the part of it that sits inside one run.
#[must_use]
pub fn matching_lines(doc: &Doc, pattern: &str) -> Vec<usize> {
    let needle = pattern.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    doc.lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.text().to_lowercase().contains(&needle))
        .map(|(idx, _)| idx)
        .collect()
}

/// Repaint every occurrence of `pattern` inside `doc` as [`Ink::Match`].
///
/// Splits runs, never lines, so the document's line count — and therefore
/// the cursor, the scroll offset and every hit's line number — is exactly
/// the same before and after highlighting.
pub fn highlight(doc: &mut Doc, pattern: &str) {
    let needle = pattern.trim().to_lowercase();
    if needle.is_empty() {
        return;
    }
    for line in &mut doc.lines {
        let mut out: Vec<Run> = Vec::new();
        for run in line.runs.drain(..) {
            out.extend(split_on(run, &needle));
        }
        line.runs = out;
    }
}

/// One run, split into the parts of it that match `needle` and the parts
/// that do not.
/// Case folding runs *forwards*, per source character, carrying each folded
/// character back to the byte range of the character it came from — never by
/// searching a `to_lowercase()` copy and using its byte offsets against the
/// original. Lowercasing is not length-preserving (`İ` folds to two code
/// points; `K` U+212A folds from three bytes to one), so an offset taken from
/// the folded copy can land inside a character of the original: at best a
/// mangled highlight, at worst a panic on a slice boundary. Folding forwards
/// has neither failure mode, and it makes a match that begins part-way into
/// one character's expansion highlight that whole character, which is the
/// only thing that could be drawn anyway.
fn split_on(run: Run, needle: &str) -> Vec<Run> {
    // (start byte, end byte, folded char) per folded character. A character
    // whose lowercase form is several characters contributes several entries,
    // each naming its own source character's byte range.
    let folded: Vec<(usize, usize, char)> = run
        .text
        .char_indices()
        .flat_map(|(at, c)| {
            let end = at + c.len_utf8();
            c.to_lowercase().map(move |lower| (at, end, lower))
        })
        .collect();
    let wanted: Vec<char> = needle.chars().collect();
    if wanted.is_empty() || folded.len() < wanted.len() {
        return vec![run];
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut idx = 0;
    while idx + wanted.len() <= folded.len() {
        let hit = folded
            .get(idx..idx + wanted.len())
            .is_some_and(|window| window.iter().map(|(_, _, c)| *c).eq(wanted.iter().copied()));
        if !hit {
            idx += 1;
            continue;
        }
        let start = folded.get(idx).map_or(0, |(start, _, _)| *start);
        let end = folded
            .get(idx + wanted.len() - 1)
            .map_or(start, |(_, end, _)| *end);
        // A hit whose source characters overlap the previous hit's is already
        // drawn; pushing it would slice the same bytes twice.
        let overlaps = ranges.last().is_some_and(|(_, last_end)| start < *last_end);
        if !overlaps {
            ranges.push((start, end));
        }
        idx += wanted.len();
    }
    if ranges.is_empty() {
        return vec![run];
    }

    let mut out = Vec::new();
    let mut at = 0;
    for (start, end) in ranges {
        if let Some(before) = run.text.get(at..start).filter(|text| !text.is_empty()) {
            out.push(Run {
                text: before.to_owned(),
                ..run.clone()
            });
        }
        if let Some(hit) = run.text.get(start..end) {
            out.push(Run {
                text: hit.to_owned(),
                ink: Ink::Match,
                ..run.clone()
            });
        }
        at = end;
    }
    if let Some(tail) = run.text.get(at..).filter(|text| !text.is_empty()) {
        out.push(Run {
            text: tail.to_owned(),
            ..run
        });
    }
    out
}
