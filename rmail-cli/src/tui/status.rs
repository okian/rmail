//! The status bar, and what the daemon heartbeat puts in it (task 92).
//!
//! # Zones, not a sentence
//!
//! Task 83's bar was one line of concatenated fragments: a message, then a
//! busy marker, then a mode, then whatever was half-typed. That reads fine
//! until the message is long — and `Model::status` is unbounded by design (an
//! SMTP server's verbatim rejection reaches it) — at which point every fixed
//! fact after it is pushed off the row it was there to explain.
//!
//! So the bar is zones with declared widths, and only the message zone
//! flexes. The mode is in the same columns whether or not anything failed, and
//! a daemon that has stopped indexing is visible while a two-hundred-character
//! rejection is on screen.
//!
//! # All ten modes, and no table to keep in step
//!
//! The label is derived from [`Mode::id`] — the same string `keys.toml` names
//! the layer by — rather than chosen per mode here. Task 83 labelled three of
//! them and fell through to nothing for the rest, so `Mode::Pick`,
//! `Mode::Confirm`, `Mode::Help` and `Mode::Prompt` were all indistinguishable
//! from `Mode::Normal` on the bar. Deriving means a mode a later task adds
//! shows up without an edit here, and `Mode::Prompt` stops being labelled
//! `INSERT` — which it is not: they are different layers with different
//! bindings, and the bar saying so is how somebody works out why `<tab>` did
//! something unexpected.
//!
//! # The heartbeat, and why it must not touch `inflight`
//!
//! [`Daemon`] is what the heartbeat has learned. `Model::inflight` counts work
//! *the user asked for*, and the busy marker is the whole reason it is
//! tracked; a five-second poll that incremented it would leave the marker on
//! forever and destroy the one signal it carries. So the heartbeat's `Cmd` is
//! deliberately outside the `inflight` bookkeeping, and
//! `tests::a_heartbeat_never_touches_the_inflight_count` is what keeps it
//! there.
//!
//! # Facts here, colours in `tui::view`
//!
//! [`HealthState`] names what a subsystem is *doing*; the glyph and the tone
//! are how it reads. The tone is `report::ReportTone` rather than a second
//! severity vocabulary — one scale for "healthy / worth noticing / wrong",
//! shared with the Report overlay, because two scales would eventually
//! disagree about what yellow means. The glyph rides along for the reason task
//! 90's rows carry one: colour alone is not a signal on a monochrome terminal
//! or to a red-green colour-blind reader.

#[cfg(test)]
mod tests;

use rmail_core::command;
use rmail_core::keymap::{Action, Mode};

use super::model::{Focus, Model, Screen};
use super::report::ReportTone;

/// What a subsystem is doing, as the heartbeat last saw it.
///
/// Deliberately not a `bool`: "paused" and "switched off in config" and "could
/// not be asked" are three different answers, and a bar that drew them the
/// same way would send somebody to resume a subsystem that was never enabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HealthState {
    /// Not asked yet, or the first answer has not arrived.
    #[default]
    Unknown,
    /// Doing what it should, with nothing outstanding.
    Ok,
    /// Working through a backlog. Not a fault — the opposite.
    Busy,
    /// Stopped by an operator, and resumable.
    Paused,
    /// Switched off in configuration. Not a fault and not resumable by an RPC.
    Off,
    /// Past a soft limit, or failing some of its work.
    Strained,
    /// Blocked, or could not be asked at all.
    Failed,
}

impl HealthState {
    /// The one-character glyph this state draws with.
    ///
    /// Distinct per state and one cell wide — `tests::every_state_has_its_own`
    /// single-width glyph holds both, because a duplicate glyph would make the
    /// colour load-bearing again and a double-width one would shift the zone
    /// after it.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Unknown => "?",
            Self::Ok => "✓",
            Self::Busy => "↻",
            Self::Paused => "‖",
            Self::Off => "·",
            Self::Strained => "!",
            Self::Failed => "✗",
        }
    }

    /// How it reads, on the scale the Report overlay already uses.
    #[must_use]
    pub const fn tone(self) -> ReportTone {
        match self {
            // Neither good nor bad: nobody has answered yet.
            Self::Unknown => ReportTone::Plain,
            Self::Ok => ReportTone::Ok,
            // Busy is healthy, and dimmer than healthy-and-idle so a glance
            // across the zone finds the states that want attention.
            Self::Busy | Self::Off => ReportTone::Muted,
            Self::Paused | Self::Strained => ReportTone::Warn,
            Self::Failed => ReportTone::Bad,
        }
    }
}

/// One subsystem's standing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Health {
    /// What it is doing.
    pub state: HealthState,
    /// One line of detail — "12 folders", "queue 3", "$0.12 today".
    ///
    /// Formatted at the wire seam rather than carried as numbers, for the
    /// reason `overlays::Explanation` gives: `Model` is compared with
    /// `assert_eq!` throughout its tests, and a `f64` in it would cost `Eq` on
    /// every enum that reaches it for no gain a renderer can use.
    pub detail: String,
}

impl Health {
    /// A subsystem that answered.
    #[must_use]
    pub fn new(state: HealthState, detail: impl Into<String>) -> Self {
        Self {
            state,
            detail: detail.into(),
        }
    }

    /// A subsystem that could not be asked.
    #[must_use]
    pub fn failed(why: impl Into<String>) -> Self {
        Self::new(HealthState::Failed, why)
    }
}

/// Which subsystem a heartbeat answer is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subsystem {
    /// `SyncService.Status`.
    Sync,
    /// `IndexService.Status`.
    Index,
    /// `AiService.GetUsage`.
    Ai,
    /// `AiPolicyService.GetSpend`.
    Spend,
}

impl Subsystem {
    /// Every subsystem the heartbeat asks about, in the order the bar draws
    /// them.
    pub const ALL: &'static [Self] = &[Self::Sync, Self::Index, Self::Ai, Self::Spend];

    /// The short name the indicator is labelled with.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Index => "idx",
            Self::Ai => "ai",
            Self::Spend => "$",
        }
    }

    /// The verb path whose Report expands this indicator.
    ///
    /// A path rather than a rendered string, so [`Indicator::expands`] can ask
    /// the registry whether this build actually has that verb — see its docs.
    #[must_use]
    pub const fn verb(self) -> &'static [&'static str] {
        match self {
            Self::Sync => &["sync", "status"],
            Self::Index => &["index", "status"],
            Self::Ai => &["ai", "status"],
            Self::Spend => &["ai", "budget", "status"],
        }
    }
}

/// What the heartbeat has learned about the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Daemon {
    /// `SyncService.Status`.
    pub sync: Health,
    /// `IndexService.Status`.
    pub index: Health,
    /// `AiService.GetUsage`.
    pub ai: Health,
    /// `AiPolicyService.GetSpend`.
    pub spend: Health,
}

impl Daemon {
    /// One subsystem's standing.
    #[must_use]
    pub fn get(&self, which: Subsystem) -> &Health {
        match which {
            Subsystem::Sync => &self.sync,
            Subsystem::Index => &self.index,
            Subsystem::Ai => &self.ai,
            Subsystem::Spend => &self.spend,
        }
    }

    /// Record what one subsystem answered.
    pub fn set(&mut self, which: Subsystem, health: Health) {
        *match which {
            Subsystem::Sync => &mut self.sync,
            Subsystem::Index => &mut self.index,
            Subsystem::Ai => &mut self.ai,
            Subsystem::Spend => &mut self.spend,
        } = health;
    }
}

/// One indicator in the daemon zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indicator {
    /// Which subsystem.
    pub which: Subsystem,
    /// What it is doing.
    pub state: HealthState,
    /// One line of detail, for whatever surface has room for it.
    pub detail: String,
    /// The `:` command that expands this indicator into a Report — `Some` only
    /// when this build has that verb.
    ///
    /// Asked of the verb registry rather than written down, because tasks 94
    /// and 96 are what declare `:index status` and `:ai budget status`: naming
    /// a command that does not resolve yet would be the bar telling somebody
    /// to type something that answers "unknown command". Declaring the verbs
    /// there turns these hints on with no edit here.
    pub expands: Option<String>,
}

/// The bar, zone by zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusBar {
    /// `-- NORMAL --`, and so on for all ten layers.
    pub mode: String,
    /// Account, folder, and how many loaded rows are unread.
    pub scope: String,
    /// [`Model::status`], which is the one zone that flexes.
    pub message: String,
    /// Whether [`StatusBar::message`] is a failure.
    pub failed: bool,
    /// The daemon indicators, in [`Subsystem::ALL`] order.
    pub daemon: Vec<Indicator>,
    /// Outstanding work the *user* asked for. Empty when there is none.
    pub inflight: String,
    /// What has been typed towards a binding and not resolved. Empty when
    /// nothing has.
    pub pending: String,
    /// The focus hint task 93 added, when the folder pane has focus. Non-empty
    /// means *eligible*: `tui::view` shows it only at a width where that pane
    /// is not drawn — see [`focus_hint`].
    pub focus_hint: String,
}

/// The widest a mode label can be, so its zone is genuinely fixed.
///
/// Computed from the longest [`Mode::id`] rather than measured from whatever
/// mode happens to be active: a zone whose width depends on its content is not
/// a zone, and every fact after it would move when the mode changed.
pub const MODE_WIDTH: usize = 13;

/// The width the daemon zone reserves per indicator: glyph, label, and a space.
pub const INDICATOR_WIDTH: usize = 7;

/// `-- NORMAL --` for [`Mode::Normal`], and so on for every layer.
#[must_use]
pub fn mode_label(mode: Mode) -> String {
    format!("-- {} --", mode.id().to_uppercase())
}

/// Read the bar off the model.
#[must_use]
pub fn bar(model: &Model) -> StatusBar {
    StatusBar {
        mode: mode_label(model.mode()),
        scope: scope(model),
        message: model.status.clone(),
        failed: model.level == crate::tui::model::Level::Error,
        daemon: Subsystem::ALL
            .iter()
            .map(|which| indicator(model, *which))
            .collect(),
        // Zero is drawn as nothing rather than as `0`: the marker exists to
        // say something is happening, and a permanent `0 in flight` is a
        // permanent claim that nothing is.
        inflight: if model.inflight == 0 {
            String::new()
        } else {
            format!("⧗{}", model.inflight)
        },
        pending: model.pending.label(),
        focus_hint: focus_hint(model),
    }
}

/// One indicator, with its `:` command when this build has one.
fn indicator(model: &Model, which: Subsystem) -> Indicator {
    let health = model.daemon.get(which);
    Indicator {
        which,
        state: health.state,
        detail: health.detail.clone(),
        expands: command::verb_at(which.verb()).map(|verb| format!(":{}", verb.canonical())),
    }
}

/// Account, folder, and unread.
///
/// The unread figure counts the rows *this client has loaded*, and says so,
/// because no RPC in the API reports a folder's unread total — `FolderStatus`
/// carries `message_count` and nothing else. Counting the loaded page and
/// labelling it a folder total would be a number that is wrong by however much
/// of the folder is not on screen.
fn scope(model: &Model) -> String {
    let account = model
        .current_account()
        .map_or("no account", |account| account.name.as_str());
    // The *open* folder, not the one under the folder cursor: the unread count
    // below is over `Model::messages`, which is the open folder's rows, and a
    // zone pairing one folder's name with another folder's count would be
    // wrong in the one state somebody is most likely to be in — moving the
    // folder cursor while reading the list.
    let folder = model
        .open_folder
        .and_then(|id| model.folders.iter().find(|folder| folder.id == id));
    let Some(folder) = folder.map(|folder| folder.name.as_str()) else {
        return account.to_owned();
    };
    let unread = model
        .messages
        .iter()
        .filter(|row| !row.has_flag(crate::tui::model::SEEN))
        .count();
    if unread == 0 {
        format!("{account}/{folder}")
    } else {
        format!("{account}/{folder} {unread}▾")
    }
}

/// The hint task 93 shows when the folder column is not drawn — the half of it
/// that is a fact about the model.
///
/// `render_panes` drops that column below its breakpoint without touching
/// [`Model::focus`], because the model has no terminal size to react to. Left
/// alone, a `Focus::Folders` state at that width points at a pane nothing draws.
///
/// So the condition is split: "the folder pane has focus, and a key can move it
/// away" is answered here, and "and that pane is not being drawn" stays in
/// `tui::view`, which is the only module that knows how wide the terminal is. A
/// non-empty string here is therefore *eligibility*, not a decision.
///
/// The chord is read out of the keymap rather than written as `<tab>`, which
/// fixes two things at once. A rebound `focus.toggle` was named wrongly before;
/// and in a mode that cannot reach that action at all — under the folder picker,
/// say, whose chain stops at `Global` — the hint named a key that would do
/// nothing, which is worse than no hint. An empty chord list is the honest
/// answer to both.
fn focus_hint(model: &Model) -> String {
    if model.screen != Screen::List || model.focus != Focus::Folders {
        return String::new();
    }
    let mode = model.mode();
    match model.keymap.chords_for(mode, Action::FocusToggle).first() {
        Some(chord) => format!("focus: folders ({chord})"),
        None => String::new(),
    }
}
