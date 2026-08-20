//! The form overlay: a set of named fields, edited in place, that applies as a
//! `:` line (task 96).
//!
//! # Why a form at all, when a `:` line already carries flags
//!
//! `AiPolicyService.SetBudget` replaces the whole budget for a scope: a cap the
//! request omits is a cap *cleared*, which the proto and the CLI both say in as
//! many words. So `:ai budget set --daily-hard-usd 5` typed against a budget that
//! already had a monthly cap would silently delete the monthly cap — the exact
//! shape of surprise that turns a convenience into a support ticket.
//!
//! The form is the answer to that. It opens pre-filled with what the daemon
//! currently has, so applying it sends every cap in force rather than only the
//! one that was typed; and it is visibly a *set of values being replaced*, which
//! is what the RPC actually does.
//!
//! A trailing `!` opts out and applies immediately with the CLI's own
//! replace-semantics, because somebody scripting a keybinding or repeating a line
//! from history has already decided.
//!
//! # Applying is a `:` line, not a private path to the daemon
//!
//! [`FormPane::line`] rebuilds the invocation the form is editing, with a flag
//! per field and a bang on the end. That is the property task 101 needs from
//! every settings field it adds: a keypress produces an [`Invocation`], so the
//! screen is testable by asserting the line it would run, with no daemon
//! anywhere near it — and the form cannot do something a typed line could not.
//!
//! # Modes, without a new mode
//!
//! Navigating fields is [`rmail_core::keymap::Mode::Menu`] and editing one is
//! `Mode::Insert`, derived from [`FormPane::editing`] being set, exactly as the search
//! pane derives `Prompt` from `SearchPane::typing`. So `j`/`k` move, `<enter>`
//! opens a field or applies the form, `<bs>` and ordinary characters are text,
//! and `<esc>` closes the innermost thing — the edit first, then the form.
//! Task 101's `Mode::Settings` is for a full screen with its own chain; a
//! transient overlay needs no layer of its own.

#[cfg(test)]
mod tests;

use rmail_core::command::{self, Invocation, ParsedFlag};

use super::overlays::{safe_line, truncate_chars};

/// The most characters a field holds.
///
/// A cap, not a layout: the values here are money and token counts, and a field
/// that grew without bound would be a `String` in the model driven by a held-down
/// key. Hard in both directions — [`FormPane::push`] refuses past it and
/// [`bounded`] cuts an arriving value down to it — because a value one character
/// over would be one that could never be typed back into bounds.
pub const MAX_VALUE: usize = 64;

/// A field's value: safe to draw, and no longer than [`MAX_VALUE`].
///
/// `overlays::truncate_chars` appends its ellipsis *past* `max`, which is right
/// for a table cell — the column reserves the room — and wrong here, where the
/// cap is what typing is refused at. So the ellipsis is kept but made to fit
/// inside it: a value silently cut is worse than a value visibly cut, and a cap
/// that is not a cap is worse than both.
fn bounded(value: &str) -> String {
    let safe = safe_line(value);
    if safe.chars().count() <= MAX_VALUE {
        return safe;
    }
    truncate_chars(&safe, MAX_VALUE.saturating_sub(1))
}

/// One editable field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The flag this field writes on the applied line — `daily-hard-usd`.
    ///
    /// The flag, not a free-form key, because the line is the contract: a field
    /// naming something the verb does not declare would build a line
    /// `command::parse` rejects, and `tests::every_field_names_a_flag_its_verb_declares`
    /// is what refuses that.
    pub flag: &'static str,
    /// What it is called on screen.
    pub label: &'static str,
    /// What is in it. Empty means "no cap" — which for `SetBudget` is a real
    /// value and not a missing one.
    pub value: String,
    /// One line saying what it does.
    pub hint: &'static str,
}

impl Field {
    /// A field with `value` already in it.
    #[must_use]
    pub fn new(flag: &'static str, label: &'static str, hint: &'static str, value: String) -> Self {
        Self {
            flag,
            label,
            hint,
            value: bounded(&value),
        }
    }
}

/// The field currently taking text.
///
/// Which field, not just "a field is open": the cursor and the open field are
/// separate facts, and a build where a key had been bound to `cursor.down` in
/// the `insert` layer could move one without the other. Carrying the index with
/// the value being edited makes `<esc>` put the text back where it came from
/// whatever the cursor has done since, rather than into whichever row the cursor
/// now happens to sit on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// Which field is open.
    pub at: usize,
    /// What it held when the edit began, so `<esc>` can put it back.
    ///
    /// Kept rather than re-fetched: the daemon's answer arrived once, and asking
    /// again to undo one keystroke would make `<esc>` a network round trip.
    restore: String,
}

/// A form over one verb's flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormPane {
    /// The `:` line being edited. Applying rebuilds it from the fields.
    pub invocation: Invocation,
    /// The border's title.
    pub title: String,
    /// The fields, in the order they are drawn.
    pub fields: Vec<Field>,
    /// Cursor within [`FormPane::rows`] — the fields plus the apply row.
    pub cursor: usize,
    /// The field taking text, if one is.
    pub editing: Option<Edit>,
    /// Why the last apply did not go through.
    pub error: Option<String>,
    /// Whether the daemon's pre-fill has arrived.
    ///
    /// The form opens on a read and fills in when it answers, which is the same
    /// discipline every streamed pane here follows — and it is load-bearing
    /// rather than cosmetic. Applying an unfilled form would send a request with
    /// no caps in it, and for `SetBudget` that is not "change nothing", it is
    /// *clear every cap*. So [`FormPane::blocked`] refuses until this is set,
    /// including when the read failed: a form that could not see the caps in
    /// force is exactly the form that must not replace them.
    pub ready: bool,
    /// The request that pre-filled it, so a late answer to a superseded one is
    /// recognisable.
    pub generation: u64,
}

impl FormPane {
    /// A form for `invocation`, pre-filled with `fields`.
    #[must_use]
    pub fn new(
        invocation: Invocation,
        title: impl Into<String>,
        fields: Vec<Field>,
        generation: u64,
    ) -> Self {
        Self {
            invocation,
            title: title.into(),
            fields,
            cursor: 0,
            editing: None,
            error: None,
            ready: false,
            generation,
        }
    }

    /// Take the daemon's answer: set what it reports, then let the typed line
    /// win.
    ///
    /// That order is what "flags pre-fill the form" means. The daemon supplies
    /// the caps in force so applying replaces them with themselves, and
    /// `:ai budget set --daily-hard-usd=5` then overwrites the one field it
    /// named — a starting point rather than a whole replacement.
    ///
    /// A value naming no field is ignored rather than refused: the wire seam
    /// sends what the RPC reports, and a cap this build has no field for is a
    /// newer daemon's, not a bug in this one.
    ///
    /// Returns whether the answer was for this form's own request. A late
    /// answer to a superseded one is dropped for the reason every generation
    /// stamp here exists: `update` is pure and cannot cancel anything.
    pub fn fill(&mut self, generation: u64, values: &[(String, String)]) -> bool {
        if generation != self.generation {
            return false;
        }
        for (flag, value) in values {
            if let Some(field) = self.fields.iter_mut().find(|field| field.flag == flag) {
                field.value = bounded(value);
            }
        }
        let flags = self.invocation.flags.clone();
        self.prefill(&flags);
        self.ready = true;
        self.error = None;
        true
    }

    /// The read that was to pre-fill this form failed.
    ///
    /// Leaves [`FormPane::ready`] clear, so the form stays un-appliable — see
    /// that field. The error is on screen and `Esc` closes; `!` on the `:` line
    /// is how somebody who has decided anyway proceeds.
    pub fn fail(&mut self, generation: u64, error: String) -> bool {
        if generation != self.generation {
            return false;
        }
        self.error = Some(error);
        true
    }

    /// Why this form cannot be applied yet, if it cannot.
    ///
    /// Here rather than at the keypress, so the one rule that matters about
    /// this pane — an unfilled form must not replace what it could not read —
    /// is stated once and testable without a `Model`.
    #[must_use]
    pub fn blocked(&self) -> Option<String> {
        if self.ready {
            return None;
        }
        Some(match &self.error {
            Some(error) => format!("{error} — nothing was changed"),
            None => "still reading what is in force — nothing to replace yet".to_owned(),
        })
    }

    /// How many rows the cursor moves through: every field, then the apply row.
    ///
    /// The apply row is a row rather than a key of its own because `Mode::Menu`
    /// has no spare gesture that means "commit this" — `<enter>` already means
    /// "use the highlighted row", and a form whose fields and whose commit both
    /// answered to `<enter>` needs the commit to *be* a row.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.fields.len() + 1
    }

    /// Whether the cursor is on the apply row.
    #[must_use]
    pub fn on_apply(&self) -> bool {
        self.cursor >= self.fields.len()
    }

    /// The highlighted field, when the cursor is on one.
    #[must_use]
    pub fn field(&self) -> Option<&Field> {
        self.fields.get(self.cursor)
    }

    /// Start editing the highlighted field.
    ///
    /// Nothing happens on the apply row — there is no field there to `get`, and
    /// putting the keyboard into insert mode over it would leave somebody typing
    /// into a button.
    pub fn edit(&mut self) {
        let Some(field) = self.fields.get(self.cursor) else {
            return;
        };
        self.editing = Some(Edit {
            at: self.cursor,
            restore: field.value.clone(),
        });
        self.error = None;
    }

    /// Stop editing, keeping what was typed.
    pub fn commit(&mut self) {
        self.editing = None;
    }

    /// Stop editing, putting back what was there before.
    ///
    /// Returns whether an edit was in progress, so the caller can tell "the edit
    /// was cancelled" from "there was nothing to cancel and the form itself
    /// should close" — the innermost-thing-first rule `leave` follows everywhere.
    pub fn cancel_edit(&mut self) -> bool {
        let Some(edit) = self.editing.take() else {
            return false;
        };
        if let Some(field) = self.fields.get_mut(edit.at) {
            field.value = edit.restore;
        }
        true
    }

    /// Append to the open field.
    pub fn push(&mut self, ch: char) {
        let Some(field) = self.open_field() else {
            return;
        };
        if field.value.chars().count() >= MAX_VALUE {
            return;
        }
        field.value.push(ch);
        self.error = None;
    }

    /// Delete the last character of the open field.
    pub fn backspace(&mut self) {
        if let Some(field) = self.open_field() {
            field.value.pop();
            self.error = None;
        }
    }

    /// The field an edit is open on, for the two calls that write to one.
    fn open_field(&mut self) -> Option<&mut Field> {
        let at = self.editing.as_ref()?.at;
        self.fields.get_mut(at)
    }

    /// The `:` line this form applies: the verb, a flag per non-empty field, and
    /// a bang.
    ///
    /// Bang'd because the form *is* the deliberate act — it opened, it was
    /// read, it was applied — and re-entering the same dispatch without one
    /// would open a second form over the first.
    ///
    /// An empty field contributes no flag, which is how "no cap" is expressed:
    /// `SetBudget` replaces the whole scope, so a cap the line omits is a cap
    /// cleared. That is the same rule `mail ai budget set` follows, and the
    /// reason the form pre-fills at all.
    #[must_use]
    pub fn line(&self) -> String {
        let mut line = self.invocation.verb.join(" ");
        for positional in &self.invocation.positionals {
            line.push(' ');
            line.push_str(&quoted(positional));
        }
        // Every flag the fields do not own, carried through verbatim.
        // `--account` and `--bulk` choose *which* budget is being replaced, and
        // a form that dropped them would replace the global one however it was
        // opened — a wrong answer that looks like the right one.
        for flag in &self.invocation.flags {
            if self.fields.iter().any(|field| field.flag == flag.name) {
                continue;
            }
            match &flag.value {
                Some(value) => line.push_str(&format!(" --{}={}", flag.name, quoted(value))),
                None => line.push_str(&format!(" --{}", flag.name)),
            }
        }
        for field in &self.fields {
            let value = field.value.trim();
            if value.is_empty() {
                continue;
            }
            line.push_str(&format!(" --{}={}", field.flag, quoted(value)));
        }
        line.push('!');
        line
    }

    /// The invocation [`FormPane::line`] parses to.
    ///
    /// Parsed rather than assembled, so applying a form and typing the same line
    /// are the same code path from here on — including the flag validation, which
    /// is how a field holding something the verb will not accept is refused at
    /// the form rather than a round trip later.
    ///
    /// # Errors
    ///
    /// The parser's own complaint, which names the offending flag or value.
    pub fn apply(&self) -> Result<Invocation, command::CommandError> {
        match command::parse(&self.line())? {
            command::Resolution::Invocation(invocation) => Ok(*invocation),
            // Unreachable: the line names the verb this form was opened for, so
            // it resolves to that verb. Reported as a parse failure rather than
            // unwrapped, because a client holding a terminal in raw mode must
            // not panic.
            command::Resolution::Children { path, .. } => Err(command::CommandError::UnknownVerb {
                path: path.join(" "),
                suggestion: None,
            }),
        }
    }

    /// Pre-fill from the flags a `:` line already carried.
    ///
    /// What "flags pre-fill the form" means: `:ai budget set --daily-hard-usd 5`
    /// opens the form with that field already holding `5` and the rest holding
    /// what the daemon had — so the line somebody typed is a starting point
    /// rather than a whole replacement.
    pub fn prefill(&mut self, flags: &[ParsedFlag]) {
        for flag in flags {
            let Some(value) = flag.value.as_deref() else {
                continue;
            };
            if let Some(field) = self.fields.iter_mut().find(|field| field.flag == flag.name) {
                field.value = bounded(value);
            }
        }
    }
}

/// One token of a rebuilt `:` line, quoted when it needs to be.
///
/// A field takes whatever was typed into it, which includes spaces and quotes;
/// pasting that into a line unquoted would either split one value into two
/// tokens or end the line early. Quoted the way `command::tokenize` reads it
/// back — `"` around, `\"` for an embedded one — so what comes out of the
/// parser is what went into the field.
fn quoted(value: &str) -> String {
    if !value.is_empty() && !value.contains([' ', '\t', '"']) {
        return value.to_owned();
    }
    format!("\"{}\"", value.replace('"', "\\\""))
}
