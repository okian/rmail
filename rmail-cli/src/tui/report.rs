//! The Report overlay: one screen for every `:` verb that answers with rows
//! rather than with a message (task 90).
//!
//! # Why one overlay and not one per domain
//!
//! Tasks 94–100 put roughly two hundred RPCs behind `:` verbs. Almost all of
//! them answer the same shape of question — "what is the index doing", "which
//! rules exist", "what did the agent do last night" — and the honest reading
//! of that is that they need *one* table, not two hundred screens. A screen
//! per domain would mean a per-domain cursor, a per-domain Esc, a per-domain
//! supersession rule and a per-domain confirmation gate, each free to be
//! subtly different from the others; the outbox pane is the one such screen
//! this build already has, and its row colouring lives in `tui::view` keyed
//! off a `&str` state, which is precisely the shape that does not generalize.
//!
//! So a report is *data*: the columns it draws, the rows it has so far, and
//! the [`Invocation`] that produced it. Everything a report can do — move the
//! cursor, run a row, re-run itself, be superseded, be cancelled — is one
//! implementation shared by every verb that opens one.
//!
//! # Line, table and stream are the same thing here
//!
//! A "line" result is a report with one column; a "table" is a report with
//! several; a "stream" is a report whose frames keep arriving. Nothing in
//! [`ReportPane`] distinguishes them, which is the point: `:auth status`
//! answers in one unary frame and `:index rebuild` (task 94) will answer in
//! fifty, and the pane cannot tell — it applies frames until one says
//! [`ReportPane::complete`].
//!
//! # Append or replace, and why the choice belongs to the frame
//!
//! The two existing streamed panes disagree, both correctly.
//! `overlays::SearchPane` appends, because `SearchService.Search` sends each
//! hit once in rank order. `overlays::FinderPane` replaces, because
//! `FinderService.Find` sends *snapshots* — a bounded top-K heap can evict an
//! entry it already sent, so appending would keep showing rows that are no
//! longer results. A generic report has to serve both, so the frame says
//! which it is ([`ReportFill`]) and the pane obeys.
//!
//! Progress reporting is the [`ReportFill::Replace`] case rather than a
//! third, row-keyed one: a progress table is a handful of rows re-sent, and a
//! keyed update would be a second merge rule to keep in step with this one
//! for no behaviour the snapshot does not already have.
//!
//! # Supersession
//!
//! Same discipline as the other streamed panes, for the same reason: `update`
//! is pure and cannot cancel anything, so every frame carries the
//! [`ReportPane::generation`] its request was stamped with and a frame from an
//! older one is dropped. `r` ([`rmail_core::keymap::Action::ReportRerun`])
//! restarts the pane under a new generation, which is what makes a re-run
//! immune to the previous run's tail.
//!
//! The client also aborts the superseded task (`tui::grpc`'s `reporting`
//! slot) and `Esc` fires `Cmd::CancelStream` — the generation stamp handles a
//! stale *frame*, and only cancellation handles a stale *stream*, which for a
//! streaming verb is real work on the daemon nobody is going to read.
//!
//! # The confirmation gate
//!
//! A row may carry an [`Invocation`] that `<enter>` runs. Whether that asks
//! first is read off [`rmail_core::parity::Command::effect`] — the same
//! annotation `rmaild::auth::methods` gates scopes with and task 53 gates MCP
//! tools with — rather than off a list of dangerous verbs kept here. A list
//! would be a second copy of that judgement, and the copy is what goes stale
//! the first time a capability's effect is corrected.

#[cfg(test)]
mod tests;

use rmail_core::command::Invocation;
use rmail_core::parity::Command as Capability;

use super::overlays::{safe_line, truncate_chars, MAX_ROWS};

/// The most characters a single report cell keeps, before the ellipsis
/// marking the cut.
///
/// Applied when the row is built rather than when it is drawn, so the *model*
/// never holds a cell wider than the widest terminal this could be read on — a
/// daemon answering with a megabyte in one field is bounded here and not only
/// in the renderer. The renderer fits each cell to its own column on top of
/// this; the two are not the same bound and neither implies the other.
pub const MAX_CELL: usize = 160;

/// One column of a report's fixed-width grid.
///
/// The width is the *content* width in characters, chosen by whoever built
/// the report, and every row is padded or truncated to it. Fixed rather than
/// measured from the rows: a streamed report's columns would otherwise jump
/// sideways as frames arrive, and a table that reflows while it fills is
/// unreadable in exactly the case a table is most wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportColumn {
    /// The heading, drawn once above the rows.
    pub header: String,
    /// Content width in characters.
    pub width: usize,
}

impl ReportColumn {
    /// A column with `header` and `width`.
    #[must_use]
    pub fn new(header: impl Into<String>, width: usize) -> Self {
        Self {
            header: header.into(),
            width,
        }
    }
}

/// What a row's state means, independent of how it is coloured.
///
/// Named for the *meaning* rather than for a colour, so the theme decides the
/// palette and `tui::view` is still the only module that knows a `Style`
/// exists — the same split `manual::Ink` keeps. A glyph rides along because
/// colour alone is not a signal on a monochrome terminal or to a red-green
/// colour-blind reader, and task 92's status indicators are specified the
/// same way for the same reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReportTone {
    /// Ordinary data.
    #[default]
    Plain,
    /// Present but not interesting — a default, an empty setting.
    Muted,
    /// Healthy, finished, allowed.
    Ok,
    /// Degraded, paused, near a limit.
    Warn,
    /// Failed, refused, over a limit.
    Bad,
}

impl ReportTone {
    /// The one-character prefix a row of this tone is drawn with.
    ///
    /// A space for [`ReportTone::Plain`] rather than nothing, so every row's
    /// cells start in the same column whether or not it carries a glyph.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Plain => " ",
            Self::Muted => "·",
            Self::Ok => "✓",
            Self::Warn => "!",
            Self::Bad => "✗",
        }
    }
}

/// One row of a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRow {
    /// The cells, positionally matching [`ReportPane::columns`]. A short row
    /// draws blanks for the columns it does not fill; a long one has its
    /// extra cells dropped at draw time rather than shearing the grid.
    pub cells: Vec<String>,
    /// What this row's state means.
    pub tone: ReportTone,
    /// What `<enter>` on this row runs, if anything.
    ///
    /// An [`Invocation`] rather than a closure or an [`rmail_core::keymap::Action`]
    /// so a row's behaviour is the same vocabulary the user could have typed:
    /// there is one dispatcher (`model::run_invocation`), one place a range
    /// and a bang are honoured, and a row cannot do something no `:` line
    /// can. `None` for a row that is only information.
    pub on_enter: Option<Invocation>,
    /// The row's *no* — what `report.reject` runs.
    ///
    /// A second gesture, because a row that can be accepted inline and only
    /// rejected by typing makes the safe answer the awkward one, and task 95's
    /// tag suggestions are exactly that shape: a stream of guesses where the
    /// common reply is "not that one".
    ///
    /// Named for the only thing a second gesture on a row has meant so far. A
    /// later task wanting a *third* should think hard before adding one rather
    /// than renaming this to something like `on_alt`, which would be a name
    /// that says nothing about what pressing the key does.
    pub on_reject: Option<Invocation>,
}

impl ReportRow {
    /// A plain informational row.
    #[must_use]
    pub fn new<I, S>(cells: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            cells: cells
                .into_iter()
                .map(|cell| truncate_chars(&safe_line(&cell.into()), MAX_CELL))
                .collect(),
            tone: ReportTone::Plain,
            on_enter: None,
            on_reject: None,
        }
    }

    /// The same row, tinted.
    #[must_use]
    pub fn toned(mut self, tone: ReportTone) -> Self {
        self.tone = tone;
        self
    }

    /// The same row, with something for `<enter>` to run.
    #[must_use]
    pub fn running(mut self, invocation: Invocation) -> Self {
        self.on_enter = Some(invocation);
        self
    }

    /// The same row, with something for `report.reject` to run.
    #[must_use]
    pub fn rejecting(mut self, invocation: Invocation) -> Self {
        self.on_reject = Some(invocation);
        self
    }
}

/// How a frame's rows join the ones already on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFill {
    /// Extend the list — each frame is new rows (`SearchPane`'s discipline).
    Append,
    /// Replace the list wholesale — each frame is a complete snapshot
    /// (`FinderPane`'s discipline).
    Replace,
}

/// A report: what produced it, what it draws, and what has arrived so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportPane {
    /// The `:` line this report is the answer to.
    ///
    /// Not an `Option`: a report *is* the result of an invocation, which is
    /// what makes `r` mean something on every one of them. A listing with no
    /// invocation behind it — the interior-node case `:message`, say — is not
    /// a report and belongs in the band task 91 draws for it.
    pub invocation: Invocation,
    /// The pane's own title, as the border draws it.
    pub title: String,
    /// The grid.
    pub columns: Vec<ReportColumn>,
    /// The rows so far.
    pub rows: Vec<ReportRow>,
    /// Cursor within [`ReportPane::rows`].
    pub cursor: usize,
    /// The generation the outstanding request was issued under.
    pub generation: u64,
    /// Whether the answer has finished arriving.
    pub complete: bool,
    /// Why the report failed, if it did.
    pub error: Option<String>,
    /// Whether a mutation has run since these rows arrived.
    ///
    /// A report is a view of state, and a row that changed that state leaves
    /// the rows above it describing how things were. Saying so is the honest
    /// alternative to re-reading automatically, which cannot be done correctly
    /// from here: the mutation is still in flight when the row's command
    /// returns, so a refresh issued then races it and may redraw the state from
    /// before the change — a wrong answer with no marking at all.
    pub stale: bool,
    /// Whether `r` must refuse to re-run this report (task 97).
    ///
    /// A report is normally a *view*, and `r` re-reads it. A handful of verbs
    /// instead *produce* something — `:token create` mints a token — and for
    /// those a second run is a second thing produced rather than a fresher look
    /// at the first. `commands::Request::once` is where the judgement is
    /// declared, per verb; `model::rerun_report` is what honours it.
    pub once: bool,
}

impl ReportPane {
    /// An empty report for `invocation`, waiting for its first frame.
    #[must_use]
    pub fn new(
        invocation: Invocation,
        title: impl Into<String>,
        columns: Vec<ReportColumn>,
        generation: u64,
    ) -> Self {
        Self {
            invocation,
            title: title.into(),
            columns,
            rows: Vec::new(),
            cursor: 0,
            generation,
            complete: false,
            error: None,
            stale: false,
            once: false,
        }
    }

    /// The same report, marked un-re-runnable. See [`ReportPane::once`].
    #[must_use]
    pub fn only_once(mut self) -> Self {
        self.once = true;
        self
    }

    /// The highlighted row.
    #[must_use]
    pub fn row(&self) -> Option<&ReportRow> {
        self.rows.get(self.cursor)
    }

    /// Apply one frame, if it belongs to the current request.
    ///
    /// The cursor is kept where it was and clamped rather than reset, so rows
    /// arriving underneath do not move the selection out from under the
    /// reader's finger — but a cursor past the new end has to come back inside
    /// it, which is the case a [`ReportFill::Replace`] snapshot produces every
    /// time it shrinks.
    pub fn apply(
        &mut self,
        generation: u64,
        fill: ReportFill,
        rows: Vec<ReportRow>,
        complete: bool,
    ) {
        if generation != self.generation {
            return;
        }
        match fill {
            ReportFill::Append => {
                let room = MAX_ROWS.saturating_sub(self.rows.len());
                self.rows.extend(rows.into_iter().take(room));
            }
            ReportFill::Replace => {
                let mut rows = rows;
                rows.truncate(MAX_ROWS);
                self.rows = rows;
            }
        }
        self.complete = complete;
        self.clamp();
    }

    /// Record why the report could not be produced.
    ///
    /// A failure ends the report — there is no more coming — but does not
    /// clear the rows that did arrive: a rebuild that streamed forty folders
    /// and then lost its connection has told the reader something true about
    /// those forty, and blanking them to show the error would throw it away.
    pub fn fail(&mut self, generation: u64, why: String) {
        if generation != self.generation {
            return;
        }
        self.error = Some(why);
        self.complete = true;
    }

    /// Start the same report again under a new generation.
    ///
    /// Rows are cleared, unlike `FinderPane::restart`: a finder keeps the old
    /// query's rows so typing does not strobe to empty, but a re-run is a
    /// deliberate "tell me again", and rows from the previous run sitting
    /// under a fresh header would be indistinguishable from the new answer.
    ///
    /// The cursor is *not* reset. A re-run is a refresh of the same report,
    /// and a long table that sent the reader back to row 0 every time they
    /// pressed `r` would make `r` the wrong key to press. It is left out of
    /// range of the (now empty) rows deliberately: [`ReportPane::apply`]
    /// clamps it when the new rows land, so a shorter answer brings it back
    /// inside and an equally long one leaves it where it was.
    pub fn restart(&mut self, generation: u64) {
        self.generation = generation;
        self.rows.clear();
        self.complete = false;
        self.error = None;
        self.stale = false;
    }

    /// Bring the cursor back inside the rows.
    fn clamp(&mut self) {
        self.cursor = self.cursor.min(self.rows.len().saturating_sub(1));
    }
}

/// Whether running `invocation` could change anything an observer outside this
/// process could see — [`Capability::effect`]'s question, asked of a parsed
/// line.
///
/// Two reads rather than one, and the second is deliberately redundant *today*.
/// An auto-derived verb's [`rmail_core::command::Verb::capability`] is filled
/// in from its own action, so `:message delete` already answers `true` from the
/// first read; a verb declared in `command::explicit` fills both fields by
/// hand, and nothing in the registry makes it fill the capability in when it
/// filled the action in. A declaration that named `Action::Delete` and left its
/// capability `None` would sail through a gate that asked only the first
/// question — and the failure mode is a report row expunging mail with nothing
/// asked, which is the one this gate exists for. `model::acts_on_mail` reads
/// the pair the same way for the same reason.
///
/// `tests::a_verb_declaring_an_action_but_no_capability_still_mutates` is the
/// constructed fixture that keeps the second read honest: the registry cannot
/// exercise it, so trusting the registry to would be a check that cannot fail.
#[must_use]
pub fn mutates(invocation: &Invocation) -> bool {
    let declared = invocation
        .capability
        .is_some_and(|capability| capability.effect().is_mutating());
    let through_action = invocation.action.is_some_and(|action| {
        Capability::for_action(action).any(|capability| capability.effect().is_mutating())
    });
    declared || through_action
}
