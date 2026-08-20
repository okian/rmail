//! The settings screen (task 101): every switch this build has, in one place,
//! and each one expressed as a `:` line.
//!
//! # The screen has no path to the daemon
//!
//! Every field's write is an [`Invocation`] — the same one somebody could type.
//! Nothing here builds a request, opens a stream or holds a client. That is not
//! architectural neatness: it is what makes the screen testable. A test asserts
//! that `<enter>` on *this* field produces *that* line, with no daemon anywhere,
//! and the rest — the confirmation gate, the report, the form, the refusal — is
//! `tui::model`'s single dispatcher, already tested.
//!
//! It also bounds what the screen can do. A field cannot reach a capability no
//! verb reaches, cannot skip a confirmation a verb asks for, and cannot exist
//! without a line to run: [`tests::every_line_parses`] refuses one that does not.
//!
//! # It does not show current values, and that is deliberate
//!
//! A toggle here does not know whether the thing is currently on. Asking would
//! mean a read per field on every open, a stale value between reads, and a screen
//! that cannot be tested without a daemon — three costs for one convenience that
//! is already covered better: every section's first field is the *report* that
//! answers "what is it now", and that report is the surface built to say so.
//!
//! So a section reads as "here is what this subsystem's state is (press `<enter>`)
//! and here are the switches behind it".
//!
//! # Keys does not go through the daemon at all
//!
//! Rebinding writes `keys.toml` through `rmail_core::keymap::file`, which is why
//! `:keys set` is hand-written in `tui::model`'s dispatcher rather than being a
//! capability. It has to work with the daemon down — a keymap you cannot fix
//! because a socket is missing is a keymap you are stuck with — and
//! `ConfigService.SetBinding` would put a network hop in front of a local file.
//!
//! # Read-only fields
//!
//! Two reasons a setting is not writable from here, and they are different facts
//! a reader needs kept apart. [`ReadOnly::ConfigFileOnly`] means it lives in the
//! master TOML and *nothing* writes it over the wire — the line renders the exact
//! block through `tui::config_block`, which names the file and offers to open it.
//! [`ReadOnly::NoRpc`] means this build has no way to change it at all, and says
//! what would have to exist.

#[cfg(test)]
mod tests;

use rmail_core::command::{self, Invocation};

/// One value a [`FieldKind::Toggle`] or [`FieldKind::Choice`] offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Option_ {
    /// What it is called on screen.
    pub label: &'static str,
    /// The `:` line that selects it.
    pub line: &'static str,
}

/// Why a setting cannot be written from this screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOnly {
    /// It lives in the master TOML and no RPC writes it.
    ///
    /// The line renders the block to paste — see `tui::config_block`.
    ConfigFileOnly {
        /// The `:` line that renders it.
        line: &'static str,
    },
    /// This build cannot change it at all.
    NoRpc {
        /// What would have to exist for it to be changeable.
        why: &'static str,
    },
}

/// What a field is, and therefore what `<enter>` does to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Two states. `<enter>` moves to the other one and runs its line.
    Toggle(
        /// On first, off second — the order the labels are drawn in.
        [Option_; 2],
    ),
    /// Several values. `<enter>` moves to the next and runs its line.
    Choice(&'static [Option_]),
    /// A number, typed into the form the line opens.
    Number {
        /// The `:` line. It opens a form rather than writing directly — see
        /// `tui::form`.
        line: &'static str,
    },
    /// Words the screen cannot know. `<enter>` opens the `:` line with this on
    /// it, for the user to finish.
    ///
    /// The one kind that runs nothing. There is no write to express as an
    /// invocation because there is no write: an address, a token label, a chord
    /// and an action are all things only the person at the keyboard has.
    Text {
        /// The verb to put on the command line.
        line: &'static str,
    },
    /// An action. `<enter>` runs it.
    Run {
        /// The `:` line.
        line: &'static str,
    },
    /// Nothing here writes it.
    ReadOnly(ReadOnly),
}

/// One setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// What it is called.
    pub label: &'static str,
    /// One line about it.
    pub hint: &'static str,
    /// What it is.
    pub kind: FieldKind,
    /// Which option is selected, for a [`FieldKind::Toggle`] or
    /// [`FieldKind::Choice`].
    ///
    /// Where the cursor is *within the field*, not what the daemon holds — the
    /// module docs say why the screen does not ask. It starts at zero and moves
    /// as `<enter>` is pressed, so the label under the cursor is always the one
    /// the last keypress selected.
    pub at: usize,
}

impl Field {
    /// A field.
    const fn new(label: &'static str, hint: &'static str, kind: FieldKind) -> Self {
        Self {
            label,
            hint,
            kind,
            at: 0,
        }
    }

    /// The options this field cycles through, if it cycles.
    ///
    /// Borrowed from the field rather than `'static`, because a toggle's pair is
    /// stored inline: two options are a `[Option_; 2]` on the variant, not a
    /// slice somewhere else.
    #[must_use]
    pub fn options(&self) -> &[Option_] {
        match &self.kind {
            FieldKind::Toggle(options) => options.as_slice(),
            FieldKind::Choice(options) => options,
            _ => &[],
        }
    }

    /// What the field currently reads as: the selected option, or its kind.
    #[must_use]
    pub fn value(&self) -> String {
        match &self.kind {
            FieldKind::Toggle(_) | FieldKind::Choice(_) => self
                .options()
                .get(self.at)
                .map_or_else(|| "-".to_owned(), |option| option.label.to_owned()),
            FieldKind::Number { line } | FieldKind::Run { line } => format!(":{line}"),
            FieldKind::Text { line } => format!(":{line} …"),
            FieldKind::ReadOnly(ReadOnly::ConfigFileOnly { .. }) => "in the config file".to_owned(),
            FieldKind::ReadOnly(ReadOnly::NoRpc { why }) => (*why).to_owned(),
        }
    }

    /// The `:` line `<enter>` runs, and the next selection.
    ///
    /// Cycling happens here rather than at the keypress so the two cannot
    /// disagree: the line returned is always the one the new selection names.
    /// [`FieldKind::Text`] returns nothing to run — see its docs.
    #[must_use]
    pub fn accept(&self) -> Accepted {
        match &self.kind {
            FieldKind::Toggle(_) | FieldKind::Choice(_) => {
                let options = self.options();
                if options.is_empty() {
                    return Accepted::Nothing;
                }
                let at = (self.at + 1) % options.len();
                Accepted::Run {
                    line: options[at].line,
                    at,
                }
            }
            FieldKind::Number { line }
            | FieldKind::Run { line }
            | FieldKind::ReadOnly(ReadOnly::ConfigFileOnly { line }) => {
                Accepted::Run { line, at: self.at }
            }
            FieldKind::Text { line } => Accepted::Type { line },
            FieldKind::ReadOnly(ReadOnly::NoRpc { why }) => Accepted::Say { why },
        }
    }
}

/// What `<enter>` on a field asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    /// Run this line, and move the field's selection here.
    Run {
        /// The `:` line.
        line: &'static str,
        /// The option it selected.
        at: usize,
    },
    /// Open the `:` line with this on it, for the user to finish.
    Type {
        /// The verb.
        line: &'static str,
    },
    /// Say why nothing happened.
    Say {
        /// The reason.
        why: &'static str,
    },
    /// Nothing at all.
    Nothing,
}

/// The invocation `line` parses to.
///
/// # Errors
///
/// The parser's own complaint. Unreachable for a declared field —
/// `tests::every_line_parses` walks all of them — and reported rather than
/// unwrapped, because a client holding a terminal in raw mode must not panic.
pub fn invocation(line: &str) -> Result<Invocation, command::CommandError> {
    match command::parse(line)? {
        command::Resolution::Invocation(invocation) => Ok(*invocation),
        command::Resolution::Children { path, .. } => Err(command::CommandError::UnknownVerb {
            path: path.join(" "),
            suggestion: None,
        }),
    }
}

/// One page of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Which accounts exist and how they authenticate.
    Accounts,
    /// Fetching mail.
    Sync,
    /// The local index the search reads.
    Index,
    /// What a model call may cost and which backend serves it.
    Ai,
    /// The injection shield and the audit ledger.
    Safety,
    /// Standing instructions that act on mail.
    Rules,
    /// Labels, and the model's guesses at them.
    Tags,
    /// Webhooks and hooks.
    Automation,
    /// What interrupts you.
    Notifications,
    /// Saved searches and smart folders.
    Saved,
    /// The keymap.
    Keys,
    /// The panes and the palette.
    Interface,
    /// Capability tokens for this daemon's own API.
    Tokens,
    /// The daemon itself.
    Daemon,
}

impl Section {
    /// Every section, in the order `<tab>` walks them.
    pub const ALL: &'static [Self] = &[
        Self::Accounts,
        Self::Sync,
        Self::Index,
        Self::Ai,
        Self::Safety,
        Self::Rules,
        Self::Tags,
        Self::Automation,
        Self::Notifications,
        Self::Saved,
        Self::Keys,
        Self::Interface,
        Self::Tokens,
        Self::Daemon,
    ];

    /// The name `:settings <section>` takes.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Accounts => "accounts",
            Self::Sync => "sync",
            Self::Index => "index",
            Self::Ai => "ai",
            Self::Safety => "safety",
            Self::Rules => "rules",
            Self::Tags => "tags",
            Self::Automation => "automation",
            Self::Notifications => "notifications",
            Self::Saved => "saved",
            Self::Keys => "keys",
            Self::Interface => "interface",
            Self::Tokens => "tokens",
            Self::Daemon => "daemon",
        }
    }

    /// The heading it draws under.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Accounts => "Accounts",
            Self::Sync => "Sync",
            Self::Index => "Index",
            Self::Ai => "AI",
            Self::Safety => "Safety & audit",
            Self::Rules => "Rules",
            Self::Tags => "Tags",
            Self::Automation => "Automation",
            Self::Notifications => "Notifications",
            Self::Saved => "Saved searches",
            Self::Keys => "Keys",
            Self::Interface => "Interface",
            Self::Tokens => "Tokens",
            Self::Daemon => "Daemon",
        }
    }

    /// The section with this name, if there is one.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|section| section.id() == id)
    }

    /// The one after this, wrapping.
    #[must_use]
    pub fn next(self) -> Self {
        let at = Self::ALL.iter().position(|section| *section == self);
        // `unwrap_or(0)` rather than a panic for a section not in `ALL`, which
        // `tests::every_section_is_in_all` refuses — a screen that panicked on
        // `<tab>` would take the terminal with it.
        let at = at.map_or(0, |at| (at + 1) % Self::ALL.len());
        Self::ALL.get(at).copied().unwrap_or(Self::Accounts)
    }

    /// This section's fields.
    #[must_use]
    pub fn fields(self) -> Vec<Field> {
        match self {
            Self::Accounts => vec![
                Field::new(
                    "accounts",
                    "which accounts exist, and which one this session is looking at",
                    FieldKind::Run {
                        line: "account list",
                    },
                ),
                Field::new(
                    "this account",
                    "its servers, its login and where its password comes from",
                    FieldKind::Run {
                        line: "account show",
                    },
                ),
                Field::new(
                    "connection",
                    "log in for real and report what the server offers",
                    FieldKind::Run {
                        line: "account test",
                    },
                ),
                Field::new(
                    "oauth token",
                    "renew it now, rather than waiting for the daemon to",
                    FieldKind::Run {
                        line: "account refresh",
                    },
                ),
                Field::new(
                    "add an account",
                    "discovers an address's settings and proposes a block; writes nothing",
                    FieldKind::Text {
                        line: "account add",
                    },
                ),
            ],
            Self::Sync => vec![
                Field::new(
                    "state",
                    "per folder: what is stored, when it last synced, what it is waiting on",
                    FieldKind::Run { line: "sync status" },
                ),
                Field::new(
                    "fetching",
                    "stop or resume the sync loop for this account",
                    FieldKind::Toggle([
                        Option_ {
                            label: "running",
                            line: "sync resume",
                        },
                        Option_ {
                            label: "paused",
                            line: "sync pause",
                        },
                    ]),
                ),
                Field::new(
                    "sync now",
                    "one pass over this account, reporting a row per folder",
                    FieldKind::Run { line: "sync now" },
                ),
            ],
            Self::Index => vec![
                Field::new(
                    "state",
                    "the queue, the backlog and what the last pass did",
                    FieldKind::Run {
                        line: "index status",
                    },
                ),
                Field::new(
                    "indexing",
                    "stop or resume the indexer without forgetting the queue",
                    FieldKind::Toggle([
                        Option_ {
                            label: "running",
                            line: "index start",
                        },
                        Option_ {
                            label: "paused",
                            line: "index stop",
                        },
                    ]),
                ),
                Field::new(
                    "drain the queue",
                    "index whatever is already enqueued",
                    FieldKind::Run { line: "index run" },
                ),
                Field::new(
                    "check for drift",
                    "compare the index against the mail it is supposed to describe",
                    FieldKind::Run {
                        line: "index verify",
                    },
                ),
                Field::new(
                    "reclaim space",
                    "drop what the index no longer needs",
                    FieldKind::Run { line: "index gc" },
                ),
                Field::new(
                    "rebuild",
                    "throw the index away and build it again — hours, and it asks first",
                    FieldKind::Run {
                        line: "index rebuild",
                    },
                ),
            ],
            Self::Ai => vec![
                Field::new(
                    "spend",
                    "today and this month, against the caps in force",
                    FieldKind::Run {
                        line: "ai budget status",
                    },
                ),
                Field::new(
                    "caps",
                    "opens a form pre-filled with the caps in force, because storing them replaces all of them",
                    FieldKind::Number {
                        line: "ai budget set",
                    },
                ),
                Field::new(
                    "backend",
                    "which provider serves a call; local is always honoured, claude is a permission",
                    FieldKind::Choice(&[
                        Option_ {
                            label: "inherit the daemon's",
                            line: "ai provider set clear",
                        },
                        Option_ {
                            label: "local (on-device)",
                            line: "ai provider set local",
                        },
                        Option_ {
                            label: "claude (hosted)",
                            line: "ai provider set claude",
                        },
                    ]),
                ),
                Field::new(
                    "dispatch",
                    "stop or resume the AI queue; a stopped queue still accepts work",
                    FieldKind::Toggle([
                        Option_ {
                            label: "running",
                            line: "ai resume",
                        },
                        Option_ {
                            label: "paused",
                            line: "ai pause",
                        },
                    ]),
                ),
                Field::new(
                    "queue",
                    "what is waiting, what failed, what was quarantined",
                    FieldKind::Run { line: "ai status" },
                ),
                Field::new(
                    "retry the failures",
                    "re-enqueue everything that gave up",
                    FieldKind::Run { line: "ai retry" },
                ),
            ],
            Self::Safety => vec![
                Field::new(
                    "scan this message",
                    "every injection signal in it, quoting what it tried; costs nothing",
                    FieldKind::Run { line: "ai scan" },
                ),
                Field::new(
                    "the ledger",
                    "every model call: what it cost, how long it took, what left the machine",
                    FieldKind::Run { line: "ai audit" },
                ),
                Field::new(
                    "failed calls",
                    "the same ledger, narrowed to what did not work",
                    FieldKind::Run {
                        line: "ai audit --failed",
                    },
                ),
                Field::new(
                    "when a flag withholds actions",
                    "`ai.injection.block_actions_at` — a severity, and nothing writes it over the wire",
                    FieldKind::ReadOnly(ReadOnly::NoRpc {
                        why: "config file only; no RPC and no block renderer yet",
                    }),
                ),
            ],
            Self::Rules => vec![
                Field::new(
                    "rules",
                    "what exists, and whether each is enabled",
                    FieldKind::Run { line: "rule list" },
                ),
                Field::new(
                    "dry run",
                    "evaluate the enabled rules over the selection and apply nothing",
                    FieldKind::Run { line: "rule run" },
                ),
                Field::new(
                    "write one",
                    "say what it should do; you get its TOML and a dry run before it exists",
                    FieldKind::Text { line: "rule new" },
                ),
            ],
            Self::Tags => vec![
                Field::new(
                    "tags",
                    "every tag, and how many messages carry it",
                    FieldKind::Run { line: "tag list" },
                ),
                Field::new(
                    "suggestions",
                    "what the model would tag this message, with why; accept or reject inline",
                    FieldKind::Run { line: "tag suggest" },
                ),
                Field::new(
                    "auto-tagging",
                    "which suggestions apply themselves, and above what confidence",
                    FieldKind::Run { line: "tag rules" },
                ),
                Field::new(
                    "add a tag",
                    "creates one; --color and --sync decide how it is stored",
                    FieldKind::Text { line: "tag new" },
                ),
            ],
            Self::Automation => vec![
                Field::new(
                    "webhooks",
                    "where mail leaves this machine, and what each destination receives",
                    FieldKind::Run {
                        line: "webhook list",
                    },
                ),
                Field::new(
                    "delivery queue",
                    "what was sent, what is waiting, what gave up and why",
                    FieldKind::Run {
                        line: "webhook deliveries",
                    },
                ),
                Field::new(
                    "hooks",
                    "what runs on this machine when mail arrives",
                    FieldKind::Run { line: "hook list" },
                ),
                Field::new(
                    "add a hook",
                    "renders the block to paste — hooks live in the config file, by design",
                    FieldKind::Text { line: "hook add" },
                ),
                Field::new(
                    "add a webhook",
                    "https, or plaintext only on loopback; the signing key is a reference",
                    FieldKind::Text {
                        line: "webhook add",
                    },
                ),
            ],
            Self::Notifications => vec![
                Field::new(
                    "alerts",
                    "the live feed, as notifications fire",
                    FieldKind::Run {
                        line: "notify list",
                    },
                ),
                Field::new(
                    "this message",
                    "its tier, the threshold it was measured against, and why nothing fired",
                    FieldKind::Run {
                        line: "notify score",
                    },
                ),
                Field::new(
                    "threshold",
                    "the tier that fires a notification — config file only; this renders the block",
                    FieldKind::ReadOnly(ReadOnly::ConfigFileOnly {
                        line: "notify set --threshold=high",
                    }),
                ),
                Field::new(
                    "notifications",
                    "on or off for this daemon — config file only; this renders the block",
                    FieldKind::Toggle([
                        Option_ {
                            label: "enabled (block)",
                            line: "notify set --enabled",
                        },
                        Option_ {
                            label: "disabled (block)",
                            line: "notify set --disabled",
                        },
                    ]),
                ),
                Field::new(
                    "carry the subject",
                    "a notification that will not say what it is about is a badge",
                    FieldKind::Toggle([
                        Option_ {
                            label: "yes (block)",
                            line: "notify set --subject",
                        },
                        Option_ {
                            label: "no (block)",
                            line: "notify set --no-subject",
                        },
                    ]),
                ),
            ],
            Self::Saved => vec![
                Field::new(
                    "saved searches",
                    "what you have stored; enter on a row runs one",
                    FieldKind::Run { line: "saved list" },
                ),
                Field::new(
                    "smart folders",
                    "predicates with membership; a tagging folder changes mail on its own",
                    FieldKind::Run { line: "folder list" },
                ),
                Field::new(
                    "save a search",
                    "a name and the query it stands for",
                    FieldKind::Text { line: "saved save" },
                ),
                Field::new(
                    "make a folder",
                    "a predicate in the query operators, or `:folder compile` from a sentence",
                    FieldKind::Text { line: "folder new" },
                ),
            ],
            Self::Keys => vec![
                Field::new(
                    "the keymap",
                    "every binding, by mode; `c` on a row rebinds it",
                    FieldKind::Run { line: "help" },
                ),
                Field::new(
                    "rebind",
                    "writes keys.toml directly — it has to work with the daemon down",
                    FieldKind::Text { line: "keys set" },
                ),
                Field::new(
                    "the manual",
                    "the guides, the concepts and the generated key reference",
                    FieldKind::Run { line: "manual" },
                ),
            ],
            Self::Interface => vec![
                Field::new(
                    "theme",
                    "the colours; takes effect immediately and is not remembered across sessions",
                    FieldKind::Choice(&[
                        Option_ {
                            label: "dark",
                            line: "set theme dark",
                        },
                        Option_ {
                            label: "light",
                            line: "set theme light",
                        },
                        Option_ {
                            label: "mono",
                            line: "set theme mono",
                        },
                        Option_ {
                            label: "high-contrast",
                            line: "set theme high-contrast",
                        },
                    ]),
                ),
                Field::new(
                    "folder column",
                    "its share of the width, as a percentage",
                    FieldKind::Choice(&[
                        Option_ {
                            label: "20%",
                            line: "set folder-width 20",
                        },
                        Option_ {
                            label: "25%",
                            line: "set folder-width 25",
                        },
                        Option_ {
                            label: "30%",
                            line: "set folder-width 30",
                        },
                    ]),
                ),
                Field::new(
                    "preview column",
                    "its share of the width; the two together are bounded",
                    FieldKind::Choice(&[
                        Option_ {
                            label: "30%",
                            line: "set preview-width 30",
                        },
                        Option_ {
                            label: "40%",
                            line: "set preview-width 40",
                        },
                        Option_ {
                            label: "50%",
                            line: "set preview-width 50",
                        },
                    ]),
                ),
                Field::new(
                    "AI panel",
                    "its share of the width it is given",
                    FieldKind::Choice(&[
                        Option_ {
                            label: "30%",
                            line: "set ai-panel-width 30",
                        },
                        Option_ {
                            label: "40%",
                            line: "set ai-panel-width 40",
                        },
                    ]),
                ),
            ],
            Self::Tokens => vec![
                Field::new(
                    "tokens",
                    "metadata only — the secret is never recoverable, only revocable",
                    FieldKind::Run { line: "token list" },
                ),
                Field::new(
                    "mint one",
                    "the secret is shown once; --scope is required and repeatable",
                    FieldKind::Text {
                        line: "token create --name=",
                    },
                ),
                Field::new(
                    "this session's login",
                    "whether a password is cached for this socket, and where",
                    FieldKind::Run { line: "auth status" },
                ),
                Field::new(
                    "forget the password",
                    "clears the cached session; the next request logs in again",
                    FieldKind::Run { line: "auth clear" },
                ),
            ],
            Self::Daemon => vec![
                Field::new(
                    "sync",
                    "what the fetcher is doing",
                    FieldKind::Run { line: "sync status" },
                ),
                Field::new(
                    "index",
                    "what the indexer is doing",
                    FieldKind::Run {
                        line: "index status",
                    },
                ),
                Field::new(
                    "ai queue",
                    "what the AI pipeline is doing",
                    FieldKind::Run { line: "ai status" },
                ),
                Field::new(
                    "finder index",
                    "the jump index behind the fuzzy finder",
                    FieldKind::Run {
                        line: "finder status",
                    },
                ),
                Field::new(
                    "rebuild the finder index",
                    "cheap, unlike the mail index — it is derived from names",
                    FieldKind::Run {
                        line: "finder rebuild",
                    },
                ),
                Field::new(
                    "where the daemon is",
                    "the socket, the config file and the data directory",
                    FieldKind::ReadOnly(ReadOnly::NoRpc {
                        why: "`mail daemon status` reports it; no RPC returns the paths",
                    }),
                ),
            ],
        }
    }
}

/// The screen's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsState {
    /// Which section is showing.
    pub section: Section,
    /// Cursor within [`SettingsState::fields`].
    pub cursor: usize,
    /// The section's fields, with their selections.
    ///
    /// Owned rather than derived on every frame, because a field's `at` is state:
    /// it is where the last `<enter>` left the selection, and re-deriving the
    /// section would put it back to zero under a reader who had just moved it.
    pub fields: Vec<Field>,
}

impl SettingsState {
    /// The screen, opened on `section`.
    #[must_use]
    pub fn new(section: Section) -> Self {
        Self {
            section,
            cursor: 0,
            fields: section.fields(),
        }
    }

    /// Move to `section`, resetting the cursor.
    pub fn go(&mut self, section: Section) {
        self.section = section;
        self.cursor = 0;
        self.fields = section.fields();
    }

    /// The highlighted field.
    #[must_use]
    pub fn field(&self) -> Option<&Field> {
        self.fields.get(self.cursor)
    }
}
