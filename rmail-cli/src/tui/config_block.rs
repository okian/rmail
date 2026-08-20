//! A block of TOML the operator has to put in their own config file, and why
//! this client is not putting it there for them (task 98).
//!
//! # The presentation, not a fabricated write
//!
//! Several settings this TUI can *show* have no RPC that writes them. Hooks are
//! `[[hooks.hooks]]` blocks in the master TOML and `HookService` deliberately
//! has no Create; notification thresholds are `[notify]` and per-account
//! `notify.threshold`, and `NotificationService` deliberately has no
//! SetThreshold. Both protos say why in as many words: a setting that lives in
//! the operator's config file must not also live in a database the service would
//! then have to keep in sync with it.
//!
//! The wrong answers to that are (a) hiding the setting, which makes the TUI
//! quietly less capable than the config file for no reason a reader could see,
//! and (b) inventing a write — either an RPC that does not exist or a
//! config-file edit from a long-running interactive session. What this module
//! does instead is the third answer: render the exact block, name the exact
//! file, say when it takes effect, and offer to open it so it can be copied.
//!
//! `mail hook add` *does* edit the file, and that is right for a one-shot
//! command: it reads, appends, round-trip validates and renames, then exits. A
//! TUI holding the same file open across a session is a different proposition —
//! it has no idea what else has edited the file since it started, and the daemon
//! it is talking to has already loaded its own copy. So the TUI shows and the CLI
//! writes, and the block is the same text either way.
//!
//! # Why this is task 101's field model arriving early
//!
//! Task 101's settings screen needs exactly this for every field it cannot
//! write, which is what its `ReadOnlyReason::ConfigFileOnly` names. Building it
//! here rather than faking it for two verbs means the screen adopts a type that
//! already has a caller, and the presentation is one implementation rather than
//! two that drift — the same reasoning `tui::form` was built under in task 96.

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use crate::tui::report::{ReportRow, ReportTone};

/// Why a setting is not written from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    /// It lives in the master TOML and no RPC writes it at all.
    ///
    /// Hooks and notification thresholds. Nothing anywhere can change these
    /// over the wire, so the config file is not "the other way" — it is the only
    /// way, and the block is the whole answer.
    ConfigFileOnly,
    /// There is a way to write it over the wire, and this block is the other
    /// one.
    ///
    /// Accounts: `:account new` stores one through `AccountService.Create`, and
    /// the config file declares them too. Carries the verb, so the row can name
    /// it rather than leaving a reader to guess that an alternative exists.
    AlsoOverTheWire(&'static str),
}

/// A block of TOML, the file it belongs in, and when it takes effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigBlock {
    /// What this block is, for the status line and the copy affordance.
    pub label: String,
    /// The block itself, ready to paste.
    pub toml: String,
    /// The file it goes in.
    pub path: PathBuf,
    /// Why this client is not writing it.
    pub reason: ReadOnlyReason,
    /// One line saying when it takes effect once pasted.
    pub effect: &'static str,
}

impl ConfigBlock {
    /// A block for `label`.
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        toml: impl Into<String>,
        path: PathBuf,
        reason: ReadOnlyReason,
        effect: &'static str,
    ) -> Self {
        Self {
            label: label.into(),
            toml: toml.into(),
            path,
            reason,
            effect,
        }
    }

    /// The block as a report's rows: what to paste, where, and how.
    ///
    /// The TOML comes first and one row per line, because it is the thing being
    /// read: folded into one cell it would be elided at the column width, and a
    /// block a reader cannot see is a block they cannot check before pasting.
    /// The rows after it are the three facts that make it actionable — the file,
    /// when it takes effect, and the fact that this client will not write it.
    #[must_use]
    pub fn rows(&self) -> Vec<ReportRow> {
        let mut rows: Vec<ReportRow> = self
            .toml
            .lines()
            .map(|line| ReportRow::new([String::new(), line.to_owned()]))
            .collect();
        rows.push(ReportRow::new([
            "file".to_owned(),
            self.path.display().to_string(),
        ]));
        rows.push(
            ReportRow::new(["effect".to_owned(), self.effect.to_owned()]).toned(ReportTone::Muted),
        );
        rows.push(
            ReportRow::new([
                "written by".to_owned(),
                match &self.reason {
                    ReadOnlyReason::ConfigFileOnly => {
                        "you — nothing changes this over the wire".to_owned()
                    }
                    ReadOnlyReason::AlsoOverTheWire(verb) => {
                        format!("you, or :{verb} to store it through the API instead")
                    }
                },
            ])
            .toned(ReportTone::Warn),
        );
        rows
    }
}
