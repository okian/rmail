//! The `:` command grammar (prd.md's Neovim-style commands; task 88): a verb
//! registry, and a pure parser/completer over it.
//!
//! # One shared vocabulary, not a second one
//!
//! [`crate::keymap::Action`] is already a stable, dotted-id namespace —
//! `keys.toml` binds it, `?` renders it, the palette resolves it. This
//! module does not invent a parallel command namespace: dots and spaces are
//! the same separator, so `message.archive` and `message archive` name the
//! same verb, and **every existing [`Action::id`] is already a valid verb
//! with no registry entry written for it** — see [`registry`]. A verb
//! declared *without* an [`Action`] behind it — a future task's `:tag`,
//! `:rule`, `:ai budget` — that also carries a [`Capability`] (a
//! [`crate::parity::Command`]) is checked in `tests` against that
//! capability's own CLI spelling, so the two surfaces cannot drift apart by
//! accident; an action-backed verb is deliberately exempt (its path is the
//! action id, which predates this grammar and must stay typeable
//! regardless of what a capability's `cli()` says) — see that module's
//! `spells_like_its_capability` for exactly what is and is not checked, and
//! why.
//!
//! # Shape
//!
//! - [`Verb`] — one command: a path (`&["message", "archive"]`), the
//!   optional capability and action it reaches, and its positionals/flags.
//! - [`registry`] — every verb, auto-derived from [`Action::ALL`] plus
//!   whatever `explicit` declares. Lazily built once; nothing here differs
//!   run to run.
//! - [`parse`] — text to a [`Resolution`], or a [`CommandError`] naming the
//!   offending token, in [`crate::keymap::KeymapError`]'s own idiom.
//! - [`complete`] — the candidates for whatever is typed so far, positional
//!   by cursor position (verb path, then that verb's own flags). Task 91's
//!   WhichKey band renders this the same way it renders a pending chord's
//!   continuations — one "what can I type next" surface, two data sources.
//!
//! # What this task does not do
//!
//! Parse only. Nothing here dispatches a [`Resolution`] to a `Cmd`, opens an
//! overlay, or touches `rmail-cli` at all — that is task 89's
//! `Overlay::Command`/`run_command`. This module has to exist and be fully
//! tested first, the same way [`crate::keymap`]'s engine predates task 85's
//! overlays that key off it.

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use crate::keymap::Action;
use crate::parity::Command as Capability;

/// The largest count a range may hold, mirroring
/// [`crate::keymap::MAX_COUNT`] — a held-down digit key is a stuck key, not
/// a request to select nine thousand messages.
pub const MAX_COUNT: u32 = crate::keymap::MAX_COUNT;

// ---------------------------------------------------------------------------
// verbs
// ---------------------------------------------------------------------------

/// A positional argument a verb accepts, in declared order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Positional {
    /// What `describe`/an error names it — `"folder"`, `"query"`.
    pub name: &'static str,
    /// Whether the verb refuses without it.
    pub required: bool,
    /// Whether this one takes every remaining word.
    ///
    /// Free text: an instruction to synthesize a rule from, a pattern to grep
    /// for. An unquoted sentence is what somebody types, and a verb declaring one
    /// argument while its caller joins several is a declaration that does not
    /// describe the verb — which is what a dispatcher comparing counts against
    /// the declaration then refuses. Only ever the *last* declared positional;
    /// `tests::a_variadic_positional_is_always_the_last_one` is what holds that.
    pub rest: bool,
}

/// A `--flag` a verb accepts. Long-only, per the grammar's own rule — "no
/// `-a`": one spelling per concept, and a short form is a `clap` affordance
/// this grammar does not need to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flag {
    /// The name, without its leading `--`.
    pub name: &'static str,
    /// Whether `--name value` (true) or a bare `--name` switch (false).
    ///
    /// Both spellings of a value: `--name value` and `--name=value` parse
    /// identically, which [`bind`] is what makes true.
    pub takes_value: bool,
}

/// One command: a path in the verb registry, and what it reaches.
///
/// A leaf and an interior node are not different types — a [`Verb`] with no
/// [`Verb::action`] and no [`Verb::capability`] and no children is simply
/// unreachable, and nothing constructs one of those. What makes a path an
/// *interior* node (`:tag` alone opening a WhichKey band of its children,
/// per task 91, rather than erroring) is that no [`Verb`] in the registry
/// has exactly that path — [`parse`] tells the two cases apart by asking
/// the registry, not by a flag on this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verb {
    /// The full path, one segment per word — `&["message", "archive"]`,
    /// never the joined `"message archive"` (joining is [`Verb::canonical`],
    /// one direction only, so there is one place that decides the
    /// separator).
    pub path: Vec<&'static str>,
    /// The capability this verb reaches, if it reaches one directly. A verb
    /// with a bare [`Verb::action`] and no capability is normal — most of
    /// [`crate::keymap::Action`]'s local, UI-only actions (`cursor.down`,
    /// `help`) have no RPC behind them at all.
    pub capability: Option<Capability>,
    /// The action this verb delegates to when called with no arguments —
    /// see the module docs on task 89's dispatch rule. `None` for a verb
    /// this grammar is the *only* way to reach (no chord binds it).
    pub action: Option<Action>,
    /// Positional arguments, in the order they are read.
    pub positionals: &'static [Positional],
    /// Flags this verb accepts, valid in any position after the path.
    pub flags: &'static [Flag],
    /// A CLI spelling this verb is declared to reproduce even though it
    /// differs from [`crate::parity::Command::cli`] — the escape hatch
    /// `tests` needs for `tag-rules` (task 95's `:tag rules set`, nested,
    /// against the CLI's clap-flattened `tag-rules set`): a deliberate,
    /// declared choice to diverge, not a drift the check should catch.
    /// `None` for every verb that just spells things the way its
    /// capability already does.
    pub cli_alias: Option<&'static str>,
    /// A description for a verb reaching neither an action nor a capability
    /// — `describe`'s last resort before falling back to the bare path.
    /// `None` for every verb an action or a capability already describes;
    /// `Some` only exists so a verb like `:set`, local to the grammar with
    /// nothing behind it to borrow a sentence from, does not read as its own
    /// path twice in the generated command index.
    pub description: Option<&'static str>,
}

impl Verb {
    /// The path, space-joined — what `describe`, an error message, or
    /// completion shows a human. The one direction [`Verb::path`] is ever
    /// joined; parsing goes the other way, splitting on both `.` and ` `.
    #[must_use]
    pub fn canonical(&self) -> String {
        self.path.join(" ")
    }

    /// One line describing this verb — the action's own description if it
    /// has one, the capability's summary otherwise, this verb's own
    /// [`Verb::description`] if it was given one, and a bare statement of
    /// the path only if none of those apply (an interior node has no
    /// [`Verb`] at all, so every real [`Verb`] reaches at least one of the
    /// first three). Action first, the same precedence `tests`'
    /// `spells_like_its_capability` gives path spelling: for an auto-derived
    /// verb the action *is* the specific thing (`message.reply` and
    /// `message.forward` share one capability summary — "create a draft,
    /// optionally pre-filled..." — but each has its own action
    /// description), and a capability-only verb has no action to prefer
    /// over it anyway.
    #[must_use]
    pub fn describe(&self) -> String {
        if let Some(action) = self.action {
            return action.describe().to_owned();
        }
        if let Some(capability) = self.capability {
            return capability.summary().to_owned();
        }
        if let Some(description) = self.description {
            return description.to_owned();
        }
        self.canonical()
    }
}

/// Split `keys.toml`/an [`Action::id`]'s spelling into path segments: dots
/// and spaces are the same separator (the module docs' "one transform"),
/// and either is accepted on input. Empty segments (`"a..b"`, a leading or
/// trailing separator) are dropped rather than producing an empty path
/// element no [`Verb::path`] could ever contain.
fn split_path(text: &str) -> Vec<&str> {
    text.split(['.', ' '])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// the registry
// ---------------------------------------------------------------------------

/// Verbs declared beyond what [`Action::ALL`] gives for free.
///
/// Task 88's own job was the parser and the auto-derivation — domains get
/// their real verbs (`:tag`, `:rule`, `:ai budget`, …) from the tasks that
/// actually build what those verbs would call (94 onward), the same way
/// [`crate::keymap::Action`] grew one variant at a time rather than every
/// future task's binding being declared up front. A declaration here with
/// no task behind it yet would be exactly the half-finished state this
/// project's non-negotiables refuse.
///
/// The first two entries are task 103's, and they are the same action
/// twice. [`Action::ManualGrep`] needs a *declared* positional (`:helpgrep
/// invoice`), which an auto-derived verb has none of — the spelling
/// difference between a grammar that can describe itself and one that
/// quietly accepts an argument it never mentions. And it needs two paths:
/// `manual grep` because that is what its id spells, so `keys.toml` reads
/// `g/ = "manual.grep"` next to `<c-o> = "manual.back"` rather than one odd
/// sibling; and `helpgrep`, because that is what vim calls it and what task
/// 103's acceptance names. Declaring either suppresses the auto-derivation
/// (see [`registry`]), so both have to be written here or `manual grep`
/// would stop resolving.
///
/// Task 89 tried to add a third — `manual` with an optional `page`, so
/// `:manual archive` could carry one — and
/// `no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one` refused
/// it: a verb taking positionals must not be a strict prefix of another, or
/// the one word that collides (`grep`, here) silently means the longer verb
/// instead of being that argument. Declaring it anyway to serve a
/// convenience would have made the guard advisory. The page-name seam is
/// therefore still `rmail_cli::tui::model::open_manual_at`, called directly,
/// which is what task 102's `K`-on-a-key-reference-row does.
///
/// The third entry is task 93's `:set` — no action, no capability, and so
/// the first real verb to need [`Verb::description`]: neither of the other
/// two sources `Verb::describe` prefers has anything to say about it.
///
/// The fourth is task 102's `:keys set` — the command line's counterpart to
/// `mail keys set`, reached from the key reference's `c` row action. Local to
/// the grammar in the same way `:set` is (no action, no capability: it edits
/// `keys.toml` directly, the same file `mail keys set` does), and the first
/// verb here to actually declare a [`Flag`] — `--mode`, mirroring the CLI's
/// own, defaulted to `normal` the same way when the row being rebound is one
/// of Normal's own. [`check_flags`] already validates a flag against
/// whatever a [`Verb`] declares; nothing else needed to grow for this to
/// parse.
///
/// The last two are task 90's, and they are the first verbs here that reach a
/// [`Capability`] with **no** [`Action`] behind them — the shape
/// `tests::every_declared_verb_spells_its_capability_like_the_cli` was written
/// for and had until now no registry entry to check. `ClientAuthService` is
/// the one capability family no later task in `tasks.md` claims, and a TUI
/// that cannot answer "does this daemon want a password" has to be quit and
/// re-entered through `mail auth status` to find out; both paths spell the
/// verb exactly as `mail` does, so `spells_like_its_capability` holds with no
/// [`Verb::cli_alias`]. They also make task 90's Report reachable by typing:
/// `:auth status` renders one, and `auth clear` is the mutating row that
/// report's confirmation gate is about.
///
/// A function, not a `const` slice: [`Verb::path`] is a `Vec`, which cannot
/// appear in a `const` initializer at all (`Vec::new` allocates), so a
/// `const EXPLICIT: &[Verb] = &[]` can never actually gain an entry no
/// matter what a later task writes here — it would need this type changed
/// out from under it first. A plain function has no such ceiling.
fn explicit() -> Vec<Verb> {
    /// Optional, not required. A bare `:helpgrep` opens the same prompt the
    /// `g/` binding does, which is more useful than
    /// [`CommandError::MissingPositional`] — and, before task 89 puts a
    /// command line on screen, it is the only way the verb is reachable at
    /// all. `rmail_cli::tui::model::open_manual_grep_for` is what consumes
    /// the argument when there is one.
    const PATTERN: &[Positional] = &[Positional {
        name: "pattern",
        required: false,
        // `:helpgrep two words` searches for both, which
        // `model::run_invocation` has always done by joining them — declared
        // here now so the declaration says what the verb does.
        rest: true,
    }];
    /// `:tag new`'s optional shape.
    const TAG_NEW_FLAGS: &[Flag] = &[
        Flag {
            name: "color",
            takes_value: true,
        },
        Flag {
            name: "sync",
            takes_value: true,
        },
    ];
    /// `:tag rules set`'s knobs, spelled as `mail tag-rules set` spells them —
    /// `--disabled` and not `--off`, because two surfaces over one capability
    /// disagreeing about a flag name is the drift `crate::parity` exists to
    /// prevent. A switch because "stored but retired" is a state `SetTagRule`
    /// has and "delete the rule" is not one it offers.
    const TAG_RULE_FLAGS: &[Flag] = &[
        Flag {
            name: "mode",
            takes_value: true,
        },
        Flag {
            name: "min-conf",
            takes_value: true,
        },
        Flag {
            name: "disabled",
            takes_value: false,
        },
    ];
    /// How far back a synthesis or a backtest looks.
    const DAYS: &[Flag] = &[Flag {
        name: "days",
        takes_value: true,
    }];
    /// Narrow an evaluation to one rule by name.
    const RULE_NAME: &[Flag] = &[Flag {
        name: "rule",
        takes_value: true,
    }];
    /// `:rule correct`'s direction. Present means "this message is *not* what
    /// that prompt says"; absent means it is. A switch rather than a value,
    /// because those are the only two answers `RecordCorrection` records.
    const CORRECT_FLAGS: &[Flag] = &[Flag {
        name: "no",
        takes_value: false,
    }];
    /// `:ai budget set`'s eight caps, spelled as `mail ai budget set` spells
    /// them. Every one optional and value-taking, because an omitted cap is a
    /// cap *cleared* — `SetBudget` replaces the whole scope — which is the reason
    /// the bare verb opens a pre-filled form rather than sending what was typed.
    const BUDGET_FLAGS: &[Flag] = &[
        Flag {
            name: "account",
            takes_value: true,
        },
        Flag {
            name: "bulk",
            takes_value: false,
        },
        Flag {
            name: "daily-soft-usd",
            takes_value: true,
        },
        Flag {
            name: "daily-hard-usd",
            takes_value: true,
        },
        Flag {
            name: "daily-soft-tokens",
            takes_value: true,
        },
        Flag {
            name: "daily-hard-tokens",
            takes_value: true,
        },
        Flag {
            name: "monthly-soft-usd",
            takes_value: true,
        },
        Flag {
            name: "monthly-hard-usd",
            takes_value: true,
        },
        Flag {
            name: "monthly-soft-tokens",
            takes_value: true,
        },
        Flag {
            name: "monthly-hard-tokens",
            takes_value: true,
        },
    ];
    /// `:ai budget status` and `:ai provider status`, which read one scope.
    const SCOPE_FLAGS: &[Flag] = &[Flag {
        name: "account",
        takes_value: true,
    }];
    /// `:ai audit`'s filters, plus the switch that walks the whole ledger rather
    /// than the most recent page.
    const AUDIT_FLAGS: &[Flag] = &[
        Flag {
            name: "account",
            takes_value: true,
        },
        Flag {
            name: "model",
            takes_value: true,
        },
        Flag {
            name: "failed",
            takes_value: false,
        },
        Flag {
            name: "all",
            takes_value: false,
        },
    ];
    /// `:ai provider set`'s backend, and the scope it applies to.
    const PROVIDER_FLAGS: &[Flag] = &[Flag {
        name: "account",
        takes_value: true,
    }];
    /// `:ai confirm`'s direction: present re-withholds rather than releasing.
    const REVOKE_FLAGS: &[Flag] = &[Flag {
        name: "revoke",
        takes_value: false,
    }];
    /// One tag name. Optional so the verb stays typeable (see `KIND`); the
    /// caller refuses a missing one.
    const TAG: &[Positional] = &[Positional {
        name: "tag",
        required: false,
        rest: false,
    }];
    /// A filter-only query and the tag to apply to everything it selects.
    const QUERY_AND_TAG: &[Positional] = &[
        Positional {
            name: "query",
            required: false,
            rest: false,
        },
        Positional {
            name: "tag",
            required: false,
            rest: false,
        },
    ];
    /// A pending suggestion's `message_tag_id`, as the suggest report shows it.
    const SUGGESTION: &[Positional] = &[Positional {
        name: "id",
        required: false,
        rest: false,
    }];
    /// A tag rule's own name and the tag it applies.
    const RULE_AND_TAG: &[Positional] = &[
        Positional {
            name: "name",
            required: false,
            rest: false,
        },
        Positional {
            name: "tag",
            required: false,
            rest: false,
        },
    ];
    /// A backend name — `claude`, `local`, or `clear` to inherit again.
    const PROVIDER: &[Positional] = &[Positional {
        name: "provider",
        required: false,
        rest: false,
    }];
    /// A name, for a verb that addresses one thing it already has.
    const NAME: &[Positional] = &[Positional {
        name: "name",
        required: false,
        rest: false,
    }];
    /// Free text: an instruction to synthesize from, or a `claude_is` prompt to
    /// correct. Joined from every positional, because an unquoted sentence is
    /// what somebody types and reading only its first word is a silent
    /// truncation — the same rule `:helpgrep`'s pattern follows.
    const TEXT: &[Positional] = &[Positional {
        name: "text",
        required: false,
        rest: true,
    }];
    /// The entity kind `:index entities` lists — `email`, `phone`, `amount`.
    /// Not enumerated here: the daemon's own refusal names every kind it knows,
    /// and a copy of that list in the client is one that goes stale the first
    /// time the extractor learns a new one.
    ///
    /// *Optional*, for the reason `PATTERN` is: a required positional makes a
    /// verb unreachable by typing its own path, which
    /// `tests::every_real_verb_is_reachable_by_typing_its_own_path` refuses —
    /// and rightly, since the verb registry is also the command index and a row
    /// nobody can type is a row that documents nothing. The caller refuses a
    /// missing kind with a message naming some, which is what `mail entities`
    /// does too.
    const KIND: &[Positional] = &[Positional {
        name: "kind",
        required: false,
        rest: false,
    }];
    /// A draft's row id, shown by `:draft list` and echoed onto the reply
    /// pane once one is drafted. Optional for the same reason `KIND` is: the
    /// `commands` match arm is where "no id, no answer" is actually
    /// enforced, not the grammar.
    const DRAFT_ID: &[Positional] = &[Positional {
        name: "draft_id",
        required: false,
        rest: false,
    }];
    /// An outbox entry's row id, shown by the outbox pane. Optional for the
    /// same reason `DRAFT_ID` is.
    const OUTBOX_ID: &[Positional] = &[Positional {
        name: "outbox_id",
        required: false,
        rest: false,
    }];
    /// A follow-up's row id, shown by `:followup list` and `:waiting`.
    /// Optional for the same reason `DRAFT_ID` is.
    const FOLLOWUP_ID: &[Positional] = &[Positional {
        name: "id",
        required: false,
        rest: false,
    }];
    /// An account's row id, shown by `:account list`. Optional for the same
    /// reason `DRAFT_ID` is.
    const ACCOUNT_ID: &[Positional] = &[Positional {
        name: "account_id",
        required: false,
        rest: false,
    }];
    /// A capability token's row id, shown by `:token list` and by the row
    /// `:token create` mints. Optional for the same reason `DRAFT_ID` is.
    const TOKEN_ID: &[Positional] = &[Positional {
        name: "token_id",
        required: false,
        rest: false,
    }];
    /// The address `:account add` discovers settings for.
    const EMAIL: &[Positional] = &[Positional {
        name: "email",
        required: false,
        rest: false,
    }];
    /// `:account add`'s credential *reference* — how to obtain the password,
    /// never the password, which is what lets the discovery be verified by a
    /// real login — plus the switch that lets a model propose settings when
    /// every probe misses.
    ///
    /// Every flag here is named as `mail account add` names it, because two
    /// surfaces over one capability disagreeing about a flag name is the drift
    /// `parity` exists to prevent.
    const AUTOCONFIGURE_FLAGS: &[Flag] = &[
        Flag {
            name: "password-command",
            takes_value: true,
        },
        Flag {
            name: "password-env",
            takes_value: true,
        },
        Flag {
            name: "keychain",
            takes_value: true,
        },
        Flag {
            name: "ai",
            takes_value: false,
        },
    ];
    /// `:account new`'s servers, login and credential — the settings
    /// `:account add` discovers, as flags, so the row that applies a proposal
    /// is a `:` line somebody could have typed.
    const NEW_ACCOUNT_FLAGS: &[Flag] = &[
        Flag {
            name: "imap-server",
            takes_value: true,
        },
        Flag {
            name: "imap-port",
            takes_value: true,
        },
        Flag {
            name: "username",
            takes_value: true,
        },
        Flag {
            name: "smtp-server",
            takes_value: true,
        },
        Flag {
            name: "smtp-port",
            takes_value: true,
        },
        Flag {
            name: "password-command",
            takes_value: true,
        },
        Flag {
            name: "password-env",
            takes_value: true,
        },
        Flag {
            name: "keychain",
            takes_value: true,
        },
        Flag {
            name: "oauth",
            takes_value: true,
        },
    ];
    /// `:account login`'s provider and native-client details. `--oauth` names
    /// the provider, as `mail account login --oauth google` does.
    const OAUTH_FLAGS: &[Flag] = &[
        Flag {
            name: "oauth",
            takes_value: true,
        },
        Flag {
            name: "client-id",
            takes_value: true,
        },
        Flag {
            name: "client-secret-command",
            takes_value: true,
        },
        Flag {
            name: "scope",
            takes_value: true,
        },
        Flag {
            name: "no-browser",
            takes_value: false,
        },
    ];
    /// `:account refresh`'s one switch.
    const FORCE: &[Flag] = &[Flag {
        name: "force",
        takes_value: false,
    }];
    /// `:token create`'s label, scopes and expiry.
    const MINT_FLAGS: &[Flag] = &[
        Flag {
            name: "name",
            takes_value: true,
        },
        Flag {
            name: "scope",
            takes_value: true,
        },
        Flag {
            name: "ttl",
            takes_value: true,
        },
    ];
    vec![
        Verb {
            path: vec!["manual", "grep"],
            capability: None,
            action: Some(Action::ManualGrep),
            positionals: PATTERN,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["helpgrep"],
            capability: None,
            action: Some(Action::ManualGrep),
            positionals: PATTERN,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["set"],
            capability: None,
            // No delegate: nothing binds `set` to a chord, and the two
            // positionals below (an option name and its value) are not
            // something an `Action` can carry, the same reason `manual grep`
            // has none either. Task 93's only tunables are the pane widths
            // and the AI panel width; task 101's `Screen::Settings` is the
            // fuller surface, not a second grammar for the same option
            // names.
            action: None,
            // Both optional, like `manual grep`'s `PATTERN` — required: true
            // would make bare `set` fail to parse at all, which
            // `every_real_verb_is_reachable_by_typing_its_own_path` refuses
            // for *every* real verb, no exceptions. `rmail_cli`'s
            // `set_option` is where "an option and a value are both
            // mandatory to do anything" is actually enforced — a semantic
            // question the grammar has no business answering.
            positionals: &[
                Positional {
                    name: "option",
                    required: false,
                    rest: false,
                },
                Positional {
                    name: "value",
                    required: false,
                    rest: false,
                },
            ],
            flags: &[],
            cli_alias: None,
            description: Some(
                "resize a pane or the AI panel — both an option and a value are required",
            ),
        },
        Verb {
            path: vec!["keys", "set"],
            capability: None,
            // No delegate, for the same reason `set` has none: a chord and
            // an action are not something an `Action` can carry. Both
            // positionals optional for the same reachability rule `set`'s
            // own comment gives — `rmail_cli`'s dispatch is where "a chord
            // and an action are both mandatory" is actually enforced.
            action: None,
            positionals: &[
                Positional {
                    name: "chord",
                    required: false,
                    rest: false,
                },
                Positional {
                    name: "action",
                    required: false,
                    rest: false,
                },
            ],
            // The one flag any verb in this registry declares today — the
            // mode to bind in, mirroring `mail keys set`'s own `--mode`,
            // defaulted the same way (`normal`) when absent.
            flags: &[Flag {
                name: "mode",
                takes_value: true,
            }],
            cli_alias: None,
            description: Some("bind a chord to an action in keys.toml"),
        },
        Verb {
            path: vec!["auth", "status"],
            capability: Some(Capability::ClientAuthAuthStatus),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["auth", "clear"],
            capability: Some(Capability::ClientAuthClearPassword),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["index", "status"],
            capability: Some(Capability::IndexStatus),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["index", "run"],
            capability: Some(Capability::IndexReindex),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["index", "reindex"],
            capability: Some(Capability::IndexReindex),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["index", "start"],
            capability: Some(Capability::IndexSetPaused),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("index start"),
            description: None,
        },
        Verb {
            path: vec!["index", "stop"],
            capability: Some(Capability::IndexSetPaused),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("index stop"),
            description: None,
        },
        Verb {
            path: vec!["index", "rebuild"],
            capability: Some(Capability::IndexRebuild),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["index", "verify"],
            capability: Some(Capability::IndexVerify),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["index", "gc"],
            capability: Some(Capability::IndexGc),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["index", "entities"],
            capability: Some(Capability::IndexListEntities),
            action: None,
            // `ListEntities` refuses an empty kind, so a bare
            // `:index entities` is refused here rather than being sent — see
            // `KIND` on why the positional is optional even so.
            positionals: KIND,
            flags: &[],
            cli_alias: Some("entities"),
            description: None,
        },
        Verb {
            path: vec!["sync", "now"],
            capability: Some(Capability::SyncSyncFolder),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("sync"),
            description: None,
        },
        Verb {
            path: vec!["sync", "pause"],
            capability: Some(Capability::SyncPause),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["sync", "resume"],
            capability: Some(Capability::SyncResume),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["sync", "status"],
            capability: Some(Capability::SyncStatus),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["ai", "status"],
            capability: Some(Capability::AiGetUsage),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["ai", "cost"],
            capability: Some(Capability::AiGetUsage),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["ai", "pause"],
            capability: Some(Capability::AiSetPaused),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("ai pause"),
            description: None,
        },
        Verb {
            path: vec!["ai", "resume"],
            capability: Some(Capability::AiSetPaused),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("ai resume"),
            description: None,
        },
        Verb {
            path: vec!["ai", "retry"],
            capability: Some(Capability::AiRetryFailed),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["ai", "process"],
            capability: Some(Capability::AiAnalyzeMessage),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["finder", "rebuild"],
            capability: Some(Capability::FinderRebuildIndex),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("find"),
            description: None,
        },
        Verb {
            path: vec!["finder", "status"],
            capability: Some(Capability::FinderIndexStatus),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("find"),
            description: None,
        },
        Verb {
            path: vec!["tag", "add"],
            capability: Some(Capability::TagAddTag),
            action: None,
            positionals: TAG,
            flags: &[],
            cli_alias: Some("tag"),
            description: None,
        },
        Verb {
            path: vec!["tag", "rm"],
            capability: Some(Capability::TagRemoveTag),
            action: None,
            positionals: TAG,
            flags: &[],
            cli_alias: Some("untag"),
            description: None,
        },
        Verb {
            path: vec!["tag", "list"],
            capability: Some(Capability::TagListTags),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("tags"),
            description: None,
        },
        Verb {
            path: vec!["tag", "new"],
            capability: Some(Capability::TagCreateTag),
            action: None,
            positionals: NAME,
            flags: TAG_NEW_FLAGS,
            cli_alias: Some("tags create"),
            description: None,
        },
        Verb {
            path: vec!["tag", "bulk"],
            capability: Some(Capability::TagBulkTag),
            action: None,
            positionals: QUERY_AND_TAG,
            flags: &[],
            cli_alias: Some("tag-bulk"),
            description: None,
        },
        Verb {
            path: vec!["tag", "suggest"],
            capability: Some(Capability::TagSuggestTags),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("suggest-tags"),
            description: None,
        },
        Verb {
            path: vec!["tag", "accept"],
            capability: Some(Capability::TagResolveSuggestion),
            action: None,
            positionals: SUGGESTION,
            flags: &[],
            cli_alias: Some("accept-tags"),
            description: None,
        },
        Verb {
            path: vec!["tag", "reject"],
            capability: Some(Capability::TagResolveSuggestion),
            action: None,
            positionals: SUGGESTION,
            flags: &[],
            cli_alias: Some("reject-tags"),
            description: None,
        },
        Verb {
            path: vec!["tag", "rules"],
            capability: Some(Capability::TagListTagRules),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("tag-rules list"),
            description: None,
        },
        Verb {
            path: vec!["tag", "rules", "set"],
            capability: Some(Capability::TagSetTagRule),
            action: None,
            positionals: RULE_AND_TAG,
            flags: TAG_RULE_FLAGS,
            cli_alias: Some("tag-rules set"),
            description: None,
        },
        Verb {
            path: vec!["rule", "list"],
            capability: Some(Capability::RuleListRules),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["rule", "new"],
            capability: Some(Capability::RuleSynthesizeRule),
            action: None,
            positionals: TEXT,
            flags: DAYS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["rule", "add"],
            capability: Some(Capability::RuleCreateRule),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["rule", "run"],
            capability: Some(Capability::RuleEvaluateRules),
            action: None,
            positionals: &[],
            flags: RULE_NAME,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["rule", "backtest"],
            capability: Some(Capability::RuleBacktestRule),
            action: None,
            positionals: NAME,
            flags: DAYS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["ai", "budget", "status"],
            capability: Some(Capability::AiPolicyGetSpend),
            action: None,
            positionals: &[],
            flags: SCOPE_FLAGS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["ai", "budget", "set"],
            capability: Some(Capability::AiPolicySetBudget),
            action: None,
            positionals: &[],
            flags: BUDGET_FLAGS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["ai", "provider", "status"],
            capability: Some(Capability::AiPolicyGetAiProvider),
            action: None,
            positionals: &[],
            flags: SCOPE_FLAGS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["ai", "provider", "set"],
            capability: Some(Capability::AiPolicySetAiProvider),
            action: None,
            positionals: PROVIDER,
            flags: PROVIDER_FLAGS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["ai", "scan"],
            capability: Some(Capability::AiSafetyScanInjection),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: Some("ai scan-injection"),
            description: None,
        },
        Verb {
            path: vec!["ai", "confirm"],
            capability: Some(Capability::AiSafetyConfirmInjection),
            action: None,
            positionals: &[],
            flags: REVOKE_FLAGS,
            cli_alias: Some("ai scan-injection"),
            description: None,
        },
        Verb {
            path: vec!["ai", "audit"],
            capability: Some(Capability::AuditQueryAiCalls),
            action: None,
            positionals: &[],
            flags: AUDIT_FLAGS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["rule", "correct"],
            capability: Some(Capability::RuleRecordCorrection),
            action: None,
            positionals: TEXT,
            flags: CORRECT_FLAGS,
            cli_alias: None,
            description: None,
        },
        // -- reply and drafts (task 100) -------------------------------------
        Verb {
            path: vec!["reply"],
            capability: Some(Capability::ComposeDraftReply),
            // No delegate: `--ai` decides between two entirely different
            // things this verb can do — hand a message to `run_action` the
            // way `r` already does, or stream a drafted reply — and an
            // `Action` cannot carry that branch. `rmail_cli`'s dispatch is
            // where it is made, before the generic daemon-verb routing this
            // capability would otherwise take (the same early, hand-written
            // case `keys set` is).
            action: None,
            // One catch-all, joined by the caller — `--ai "yes, but push to
            // Tuesday"` needs its intent to survive as one argument the same
            // way `helpgrep`'s `PATTERN` does, not split word by word.
            positionals: PATTERN,
            flags: &[
                Flag {
                    name: "ai",
                    takes_value: false,
                },
                Flag {
                    name: "reply-all",
                    takes_value: false,
                },
            ],
            cli_alias: None,
            // `description` is dead here and stays unset: `Verb::describe`
            // prefers `capability.summary()` whenever one is set, which
            // `ComposeDraftReply` always is. What this verb does with `--ai`
            // versus without it belongs in prose a reader actually reaches —
            // see [[compose-and-send]] — not in a field nothing renders.
            description: None,
        },
        Verb {
            path: vec!["draft", "list"],
            capability: Some(Capability::ComposeListDrafts),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["draft", "show"],
            capability: Some(Capability::ComposeGetDraft),
            action: None,
            positionals: DRAFT_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["draft", "edit"],
            capability: Some(Capability::ComposeUpdateDraft),
            action: None,
            positionals: DRAFT_ID,
            flags: &[Flag {
                name: "body",
                takes_value: true,
            }],
            cli_alias: None,
            // Dead, the same reason `reply`'s is: `ComposeUpdateDraft`
            // always supplies a summary first.
            description: None,
        },
        Verb {
            path: vec!["draft", "delete"],
            capability: Some(Capability::ComposeDeleteDraft),
            action: None,
            positionals: DRAFT_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["draft", "render"],
            capability: Some(Capability::ComposeRenderDraft),
            action: None,
            positionals: DRAFT_ID,
            flags: &[],
            cli_alias: None,
            // Dead, the same reason `reply`'s is: `ComposeRenderDraft`
            // always supplies a summary first.
            description: None,
        },
        Verb {
            path: vec!["draft", "rewrite"],
            capability: Some(Capability::ComposeRewriteDraft),
            action: None,
            positionals: DRAFT_ID,
            // Mirrors `mail draft rewrite`'s own flags exactly — one
            // vocabulary for tone, spelled the way
            // `rmail_core::compose::reply::Tone::as_str` spells it, so a
            // name that works in `keys.toml`, the CLI or here works
            // everywhere.
            flags: &[
                Flag {
                    name: "tone",
                    takes_value: true,
                },
                Flag {
                    name: "shorter",
                    takes_value: false,
                },
                Flag {
                    name: "longer",
                    takes_value: false,
                },
                Flag {
                    name: "instruction",
                    takes_value: true,
                },
            ],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["draft", "revisions"],
            capability: Some(Capability::ComposeListDraftRevisions),
            action: None,
            positionals: DRAFT_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["draft", "revert"],
            capability: Some(Capability::ComposeSelectDraftRevision),
            action: None,
            // `seq` optional and defaulting to 0 (the original text) the same
            // way `mail draft revert`'s own `--seq` does.
            positionals: &[
                Positional {
                    name: "draft_id",
                    required: false,
                    rest: false,
                },
                Positional {
                    name: "seq",
                    required: false,
                    rest: false,
                },
            ],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        // -- send and the outbox (task 100) ----------------------------------
        //
        // `SendSchedulerService.WatchOutbox` is deliberately not here. Every
        // other verb below is a one-command-one-answer shape: issue a request,
        // get a `Report` or a fact back. A live tail has no such shape, and the
        // outbox pane (`O`) already re-lists on every mutation this file makes,
        // which is what a `:` line watching the stream would exist to do.
        Verb {
            path: vec!["send"],
            capability: Some(Capability::SendSchedulerScheduleSend),
            action: None,
            positionals: &[],
            // `--draft` rather than an inline `--to`/`--subject`/`--body`
            // surface `mail send` also accepts: a `:` line is one line, and
            // every message this build sends is staged as a draft first (by
            // `r`/`F`, `:reply --ai` or `:draft rewrite`) — there is always
            // one to name by id.
            flags: &[
                Flag {
                    name: "draft",
                    takes_value: true,
                },
                Flag {
                    name: "at",
                    takes_value: true,
                },
                Flag {
                    name: "undo",
                    takes_value: true,
                },
            ],
            cli_alias: None,
            // Dead, the same reason `reply`'s is: `SendSchedulerScheduleSend`
            // always supplies a summary first.
            description: None,
        },
        Verb {
            path: vec!["outbox", "retry"],
            capability: Some(Capability::SendSchedulerRetryFailed),
            action: None,
            positionals: OUTBOX_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["outbox", "reschedule"],
            capability: Some(Capability::SendSchedulerRescheduleSend),
            action: None,
            positionals: OUTBOX_ID,
            flags: &[Flag {
                name: "at",
                takes_value: true,
            }],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["outbox", "edit"],
            capability: Some(Capability::SendSchedulerUpdateScheduledBody),
            action: None,
            positionals: OUTBOX_ID,
            flags: &[Flag {
                name: "body",
                takes_value: true,
            }],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["outbox", "send-now"],
            capability: Some(Capability::SendSchedulerSendNow),
            action: None,
            positionals: OUTBOX_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["outbox", "suggest"],
            capability: Some(Capability::SendSchedulerSuggestSendTime),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        // -- follow-ups and the pre-send guardian (task 100) -----------------
        //
        // `SendSchedulerService.TrackFollowup` is deliberately not here too:
        // it judges a *sent* message's own body and recipients to pick a
        // follow-up delay, and this client has no surface — no "the message
        // I am looking at was one I sent" screen — that would let anyone name
        // one to hand it. `followup new`'s own explicit `--in` is what a
        // human names by hand instead.
        Verb {
            path: vec!["followup", "list"],
            capability: Some(Capability::SendSchedulerListFollowups),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["followup", "new"],
            capability: Some(Capability::SendSchedulerCreateFollowup),
            action: None,
            // No positional: the message is `target.message_id`, the same
            // "act on what is on screen" rule `ai process` follows — a
            // follow-up's RPC wants the message's own RFC 5322 Message-ID,
            // not the row id typeable here, so resolving it from context
            // instead of a typed argument avoids asking anybody to type a
            // header by hand.
            positionals: &[],
            flags: &[
                Flag {
                    name: "in",
                    takes_value: true,
                },
                Flag {
                    name: "note",
                    takes_value: true,
                },
            ],
            // `mail followup add` vs. this grammar's `followup new` — the
            // acceptance's own spelling, declared as a deliberate divergence
            // rather than compared against a path it was never going to
            // match.
            cli_alias: Some("followup add"),
            description: None,
        },
        Verb {
            path: vec!["followup", "dismiss"],
            capability: Some(Capability::SendSchedulerDismissFollowup),
            action: None,
            positionals: FOLLOWUP_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["waiting"],
            capability: Some(Capability::SendSchedulerListWaitingOn),
            action: None,
            positionals: &[],
            flags: &[Flag {
                name: "overdue",
                takes_value: false,
            }],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["nudge"],
            capability: Some(Capability::SendSchedulerDraftNudge),
            action: None,
            positionals: FOLLOWUP_ID,
            flags: &[],
            cli_alias: None,
            // Dead, the same reason `reply`'s is: `SendSchedulerDraftNudge`
            // always supplies a summary first.
            description: None,
        },
        Verb {
            path: vec!["preflight"],
            capability: Some(Capability::SendSchedulerPreflightCheck),
            action: None,
            positionals: DRAFT_ID,
            flags: &[],
            cli_alias: None,
            // Dead, the same reason `reply`'s is: `SendSchedulerPreflightCheck`
            // always supplies a summary first.
            description: None,
        },
        // -- accounts and tokens (task 97) -------------------------------------
        Verb {
            path: vec!["account", "list"],
            capability: Some(Capability::AccountList),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["account", "show"],
            capability: Some(Capability::AccountGet),
            action: None,
            positionals: ACCOUNT_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["account", "add"],
            capability: Some(Capability::AccountAutoconfigure),
            action: None,
            positionals: EMAIL,
            flags: AUTOCONFIGURE_FLAGS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["account", "new"],
            capability: Some(Capability::AccountCreate),
            action: None,
            positionals: NAME,
            flags: NEW_ACCOUNT_FLAGS,
            cli_alias: None,
            // `AccountCreate` has no CLI verb at all, so there is no summary
            // to inherit — see this verb's own docs in `tui::commands::account`
            // on why it is spelled `new` next to `:tag new`.
            description: Some("Add an account from settings, as `:account add` discovered them."),
        },
        Verb {
            path: vec!["account", "login"],
            capability: Some(Capability::AccountBeginOAuth),
            action: None,
            positionals: ACCOUNT_ID,
            flags: OAUTH_FLAGS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["account", "refresh"],
            capability: Some(Capability::AccountRefreshToken),
            action: None,
            positionals: ACCOUNT_ID,
            flags: FORCE,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["account", "test"],
            capability: Some(Capability::AccountTestConnection),
            action: None,
            positionals: ACCOUNT_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["account", "rm"],
            capability: Some(Capability::AccountDelete),
            action: None,
            positionals: ACCOUNT_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["account", "use"],
            // Neither, for the reason `set` has neither: switching the account
            // this session is looking at reaches no RPC, and the id it takes is
            // not something an `Action` can carry. `tui::model`'s
            // `run_invocation` is where it is answered, next to `:set`.
            capability: None,
            action: None,
            positionals: ACCOUNT_ID,
            flags: &[],
            cli_alias: None,
            description: Some("Switch which account this session is looking at."),
        },
        Verb {
            path: vec!["account", "toml"],
            // Neither, for the reason `account use` has neither: opening the
            // `[[accounts]]` block `:account add` last discovered is a client
            // affordance over session state, not a capability, and it is a verb
            // rather than a row-only gesture because a row's action *is* an
            // `Invocation` — an affordance nobody could type would document
            // nothing either.
            capability: None,
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: Some("Open the [[accounts]] block the last `:account add` discovered."),
        },
        Verb {
            path: vec!["token", "list"],
            capability: Some(Capability::AdminListTokens),
            action: None,
            positionals: &[],
            flags: &[],
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["token", "create"],
            capability: Some(Capability::AdminMintToken),
            action: None,
            positionals: &[],
            flags: MINT_FLAGS,
            cli_alias: None,
            description: None,
        },
        Verb {
            path: vec!["token", "revoke"],
            capability: Some(Capability::AdminRevokeToken),
            action: None,
            positionals: TOKEN_ID,
            flags: &[],
            cli_alias: None,
            description: None,
        },
    ]
}

/// Every verb: [`explicit`] plus one auto-derived from each [`Action`] that
/// [`explicit`] does not already cover.
///
/// Built once — nothing here is request-dependent — behind a [`OnceLock`]
/// rather than a `const`, because splitting an [`Action::id`] into path
/// segments is a runtime `str::split`, not something `const fn` can do in
/// today's Rust. The pieces themselves are still `&'static str`: slicing a
/// `&'static str` produces `&'static str` slices, so no allocation survives
/// past the `Vec<&'static str>` each [`Verb::path`] holds.
fn registry() -> &'static [Verb] {
    static REGISTRY: OnceLock<Vec<Verb>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut verbs: Vec<Verb> = explicit();
        for action in Action::ALL {
            if verbs.iter().any(|verb| verb.action == Some(*action)) {
                continue;
            }
            verbs.push(Verb {
                path: split_path(action.id()),
                capability: Capability::for_action(*action).next(),
                action: Some(*action),
                positionals: &[],
                flags: &[],
                cli_alias: None,
                description: None,
            });
        }
        verbs
    })
}

/// The verb at exactly this path, if the registry has one.
///
/// Distinguishes a real verb from an interior node: `:tag` with no verb at
/// that exact path is not [`CommandError::UnknownVerb`], it is the
/// [`Resolution::Children`] case [`parse`] returns instead — see that
/// variant's docs.
#[must_use]
pub fn verb_at(path: &[&str]) -> Option<&'static Verb> {
    registry().iter().find(|verb| verb.path == path)
}

/// Every verb whose path is strictly longer than `prefix` and starts with
/// it.
///
/// The completion primitive: task 91's WhichKey band, told to render
/// verb-path completions, is this list grouped by each member's next
/// segment — the same "longest common prefix of the member ids" derivation
/// task 91's own `Keymap::continuations` (not yet written) uses for chords,
/// applied to verb paths instead of chord bindings.
#[must_use]
pub fn children_of(prefix: &[&str]) -> Vec<&'static Verb> {
    registry()
        .iter()
        .filter(|verb| verb.path.len() > prefix.len() && verb.path[..prefix.len()] == *prefix)
        .collect()
}

// ---------------------------------------------------------------------------
// ranges
// ---------------------------------------------------------------------------

/// The message set a command applies to — vim's range grammar, the one
/// place it has a genuine mail analogue (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// `'<,'>` — the active visual selection.
    Selection,
    /// `%` — every row in the current listing.
    All,
    /// A bare leading count — `N` messages from the cursor down. Saturates
    /// at [`MAX_COUNT`], the same policy [`crate::keymap::Pending`] applies
    /// to a chord's count for the same reason: a held-down digit key is not
    /// a request to allocate.
    Count(u32),
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selection => f.write_str("'<,'>"),
            Self::All => f.write_str("%"),
            Self::Count(n) => write!(f, "{n}"),
        }
    }
}

// ---------------------------------------------------------------------------
// invocation
// ---------------------------------------------------------------------------

/// One flag as parsed: its name and, for a value-taking flag, the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFlag {
    /// The flag's name, without `--`.
    pub name: String,
    /// The value, for a flag [`Flag::takes_value`] declares one for.
    pub value: Option<String>,
}

/// A parsed, ready-to-dispatch `:` line — or, when no exact verb sits at
/// the resolved path, the interior-node case: a prompt naming what could
/// come next rather than an error (module docs' resolution algorithm, step
/// 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// An exact verb, with its arguments.
    Invocation(Box<Invocation>),
    /// `path` matched no exact verb, but is a strict prefix of at least
    /// one — `children` names every one of those; callers needing an order
    /// sort themselves. Never empty: an empty `children` would mean `path`
    /// matched nothing at all, which is [`CommandError::UnknownVerb`]
    /// instead.
    Children {
        /// The path typed so far.
        path: Vec<String>,
        /// Every verb this path is a strict prefix of.
        children: Vec<&'static Verb>,
    },
}

/// A parsed, ready-to-dispatch `:` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The range prefix, if one was typed.
    pub range: Option<Range>,
    /// The verb's path — kept as owned segments rather than borrowing
    /// [`Verb::path`], so an [`Invocation`] does not tie its caller to the
    /// registry's lifetime for what is, after parsing, just data.
    pub verb: Vec<String>,
    /// The capability this invocation's verb reaches, if any.
    pub capability: Option<Capability>,
    /// The action this invocation's verb reaches, if any.
    pub action: Option<Action>,
    /// Positional arguments, in the order they were typed.
    pub positionals: Vec<String>,
    /// Flags, in the order they were typed.
    pub flags: Vec<ParsedFlag>,
    /// Whether a trailing `!` was present — task 89's "skip the
    /// confirmation overlay," and the *only* thing `!` means (module docs:
    /// "It never changes what a command does").
    pub bang: bool,
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Why a `:` line could not be parsed.
///
/// Every variant names the offending text, in [`crate::keymap::KeymapError`]'s
/// own idiom — these are read by someone who just typed the line, and
/// "invalid command" would leave them guessing which word.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    /// Nothing but a range, or nothing at all.
    #[error("a command needs a verb")]
    Empty,
    /// A `'` that does not start the one range mark this grammar knows
    /// (`'<,'>`) — [`Range::Selection`]/[`Range::All`]/a count parse
    /// unconditionally, so this is specifically the range *token* itself
    /// being malformed, e.g. `'<,` with no closing `'>`, rather than a
    /// range with nothing to apply to (that is a caller's problem — task
    /// 89's, against a model that may have no visual selection — not a
    /// parse error).
    #[error("{text:?} looks like a range but is not '<,'>")]
    MalformedRange {
        /// The offending text, as written.
        text: String,
    },
    /// A `"` with no matching close.
    #[error("{text:?} has an unterminated quote")]
    UnterminatedQuote {
        /// The word being built when the quote failed to close — what was
        /// read after the opening `"` (plus anything glued before it), not
        /// the whole line, so the message points at the actual offending
        /// text.
        text: String,
    },
    /// No verb in the registry matches, and none has this as a strict
    /// prefix either — what [`parse`] returns when even
    /// [`Resolution::Children`] cannot apply.
    #[error("unknown command {path:?}{}", suggestion.as_deref().map_or(String::new(), |s| format!(" — did you mean `{s}`?")))]
    UnknownVerb {
        /// The path as typed, space-joined.
        path: String,
        /// The closest known verb's canonical path, if any looked close
        /// enough to name.
        suggestion: Option<String>,
    },
    /// `--name` where `name` is not one of the resolved verb's declared
    /// [`Flag`]s.
    #[error("{flag:?} is not a flag {verb} takes{}", if valid.is_empty() {
        " — it takes no flags".to_owned()
    } else {
        format!(" — try {}", valid.join(", "))
    })]
    UnknownFlag {
        /// The verb's canonical path.
        verb: String,
        /// The flag as typed, without `--`.
        flag: String,
        /// Every flag the verb does accept, `--`-prefixed.
        valid: Vec<String>,
    },
    /// A value-taking flag with nothing after it — including the common
    /// mistake of typing `--flag value` (space-separated) rather than this
    /// grammar's `--flag=value`, which `tokenize` reads as an empty `--flag`
    /// followed by a stray positional word.
    #[error("--{flag} needs a value (write it as --{flag}=value)")]
    MissingFlagValue {
        /// The flag's name.
        flag: String,
    },
    /// Fewer positionals than the verb requires.
    #[error("{verb} needs {name} — try `{verb} <{name}>`")]
    MissingPositional {
        /// The verb's canonical path.
        verb: String,
        /// The missing positional's name.
        name: &'static str,
    },
}

// ---------------------------------------------------------------------------
// tokenizing
// ---------------------------------------------------------------------------

/// One raw token off the line, before verb/flag interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    /// A flag, `--name` or `--name=value` (the `=value` half already split
    /// off, so nothing downstream has to re-scan a token to tell the two
    /// spellings apart).
    Flag {
        name: String,
        value: Option<String>,
    },
}

/// Split a line into [`Token`]s: whitespace-separated except inside a
/// `"..."` quote, where `\"` escapes a literal quote and nothing else is
/// special — no `\n`, no `\\`, so a Windows path or a regex typed inside
/// quotes survives untouched. A quoted piece may be glued to more text
/// (`--query"a b"c` reads as one token `a bc`, the same way a shell would).
///
/// Deliberately not `.`-aware, even though a verb path is (module docs'
/// "one transform") — that has to be resolved by [`parse_verb`]/[`complete`]
/// instead, against the registry, because only they know where a verb path
/// ends and a positional or a flag value begins. Splitting bare words on
/// `.` here too would look tempting (a glued `message.archive` could
/// tokenize straight into two words) but is wrong: it would also fragment
/// `--since=2024.01.01` (a flag value) and any positional containing a
/// literal `.` (`report.pdf`, `3.14`, an email address) into several
/// tokens, silently. `tokenize` only ever sees undifferentiated words; it
/// cannot tell a verb segment from an argument, so it must not guess.
///
/// Never guesses at an unterminated quote as "assume it closes at the end
/// of the line" — that would silently produce different token boundaries
/// than the input actually has — it is [`CommandError::UnterminatedQuote`]
/// instead.
/// One value, quoted so [`parse`] reads it back unchanged.
///
/// The inverse of [`tokenize`]'s quoting, and it lives next to it for that
/// reason: a client rebuilding a `:` line from values it holds — a form applying
/// its fields, a report row carrying settings a probe discovered — needs exactly
/// the escaping the parser undoes, and a second copy of that rule somewhere else
/// is a copy that drifts.
///
/// Left alone when it needs nothing: quoting every value would make a line built
/// from one unreadable next to the same line typed by hand. Wrapped in `"` with
/// embedded quotes escaped otherwise — a value carrying a space would split into
/// two tokens, and one carrying a quote would end the line early, so either way
/// the line would parse to something nobody asked for.
///
/// An empty value comes back as `""`, which is a token: the alternative is
/// nothing at all, and a positional that vanished would shift every one after
/// it.
#[must_use]
pub fn quoted(value: &str) -> String {
    if !value.is_empty() && !value.contains([' ', '\t', '"']) {
        return value.to_owned();
    }
    format!("\"{}\"", value.replace('"', "\\\""))
}

fn tokenize(text: &str) -> Result<Vec<Token>, CommandError> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut word = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                break;
            }
            if c == '"' {
                chars.next();
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        '\\' if chars.peek() == Some(&'"') => {
                            word.push('"');
                            chars.next();
                        }
                        other => word.push(other),
                    }
                }
                if !closed {
                    return Err(CommandError::UnterminatedQuote { text: word });
                }
                continue;
            }
            word.push(c);
            chars.next();
        }
        if let Some(name) = word.strip_prefix("--") {
            let (name, value) = match name.split_once('=') {
                Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
                None => (name.to_owned(), None),
            };
            tokens.push(Token::Flag { name, value });
        } else {
            tokens.push(Token::Word(word));
        }
    }
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// range prefix
// ---------------------------------------------------------------------------

/// Strip a leading range off `text`, at the character level rather than
/// the token level.
///
/// Vim's range syntax is conventionally *glued* to what follows with no
/// space (`:5d`, `:'<,'>d`) — [`tokenize`] would read `'<,'>tag` as one
/// word, never as a range plus a verb, if range-stripping ran after
/// tokenizing. Running first, on the raw text, handles the glued form and
/// the spaced form (`:'<,'> tag`) the same way, since whatever whitespace
/// follows the range is simply left for [`tokenize`] to skip as it always
/// does.
fn strip_range(text: &str) -> Result<(Option<Range>, &str), CommandError> {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("'<,'>") {
        return Ok((Some(Range::Selection), rest));
    }
    if trimmed.starts_with('\'') {
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        return Err(CommandError::MalformedRange {
            text: trimmed[..end].to_owned(),
        });
    }
    if let Some(rest) = trimmed.strip_prefix('%') {
        return Ok((Some(Range::All), rest));
    }
    let digit_end = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if digit_end > 0 {
        // A digit run long enough to overflow `u32` (ten-plus digits) is
        // exactly the held-down-key case `MAX_COUNT`'s own docs describe,
        // not a reason to give up on parsing it as a range at all — falling
        // through to "no range" on overflow would leave a run of digits
        // sitting in front of the verb, which then fails as a whole
        // `UnknownVerb` instead of saturating the way a shorter overflow
        // already does. `u64` comfortably holds any digit run a human
        // could type or a stuck key could produce; if even that overflows,
        // `map_or` saturates the same as an ordinary `u32` overflow would.
        let count = trimmed[..digit_end].parse::<u64>().map_or(MAX_COUNT, |n| {
            u32::try_from(n).unwrap_or(u32::MAX).min(MAX_COUNT)
        });
        return Ok((Some(Range::Count(count)), &trimmed[digit_end..]));
    }
    Ok((None, trimmed))
}

/// Strip a trailing `!` off `text`, at the character level rather than the
/// token level — like [`strip_range`], this has to run before [`tokenize`]
/// discards the difference between quoted and bare text. A `!` is only a
/// bang when it is truly the last character on the line; one sitting
/// inside a `"..."` quote (`ask "what happened!"`) is the argument's own
/// text, not a bang, and quoting is the only way a user has to say so.
/// Stripping `!` off the last already-*tokenized* word instead would look
/// equivalent but is not: by the time a word exists, the quote marks that
/// would distinguish those two cases are already gone.
fn strip_bang(text: &str) -> (bool, &str) {
    let trimmed = text.trim_end();
    match trimmed.strip_suffix('!') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    }
}

// ---------------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------------

/// Parse one `:` line into a [`Resolution`].
///
/// # Errors
///
/// [`CommandError`] naming what about `text` could not be parsed.
pub fn parse(text: &str) -> Result<Resolution, CommandError> {
    let (range, rest) = strip_range(text)?;
    let (bang, rest) = strip_bang(rest);
    let tokens = tokenize(rest)?;

    // Words in order, flag values included: which words are values cannot be
    // known before the verb is, because it is the [`Verb`] that declares whether
    // a flag takes one. `parse_verb` resolves the verb from these and then
    // re-walks the ordered tokens to separate the two — see [`bind`].
    let words: Vec<String> = tokens
        .iter()
        .filter_map(|token| match token {
            Token::Word(word) => Some(word.clone()),
            Token::Flag { .. } => None,
        })
        .collect();
    parse_verb(&words, &tokens, range, bang)
}

/// The verb-resolving half of [`parse`]: longest-matching-prefix of `words`
/// against the registry (module docs' resolution algorithm, step 3), the
/// rest as positionals.
///
/// Each candidate prefix is dot-expanded ([`split_path`]) before the
/// lookup, so a fully glued `message.archive` (one [`tokenize`] word) and a
/// spaced `message archive` (two) resolve identically — but only while
/// still searching for the verb. Once one is found, the positionals are
/// `words[split..]` unchanged: [`tokenize`]'s own words, dots and all, so
/// `message copy report.pdf` keeps `report.pdf` as one argument rather than
/// splitting it into two.
fn parse_verb(
    words: &[String],
    tokens: &[Token],
    range: Option<Range>,
    bang: bool,
) -> Result<Resolution, CommandError> {
    if words.is_empty() {
        return Err(CommandError::Empty);
    }

    // Longest prefix that is an exact verb wins — `tag rules set` beats
    // `tag` plus two positionals, because a longer real verb is always a
    // more specific match than treating its own trailing segments as
    // arguments.
    for split in (1..=words.len()).rev() {
        let flat: Vec<&str> = words[..split].iter().flat_map(|w| split_path(w)).collect();
        if let Some(verb) = verb_at(&flat) {
            let (flags, positionals) = bind(verb, tokens, split)?;
            check_flags(verb, &flags)?;
            check_positionals(verb, &positionals)?;
            return Ok(Resolution::Invocation(Box::new(Invocation {
                range,
                verb: verb.path.iter().map(|s| (*s).to_owned()).collect(),
                capability: verb.capability,
                action: verb.action,
                positionals,
                flags,
                bang,
            })));
        }
    }

    let flat: Vec<&str> = words.iter().flat_map(|w| split_path(w)).collect();
    let children = children_of(&flat);
    if !children.is_empty() {
        return Ok(Resolution::Children {
            path: words.to_vec(),
            children,
        });
    }

    Err(CommandError::UnknownVerb {
        path: words.join(" "),
        suggestion: closest(&words.join(" ")),
    })
}

/// Split the tokens after the verb path into its flags and its positionals,
/// pairing a value-taking flag with the word that follows it.
///
/// `--sync imap` and `--sync=imap` are the same thing, which is what
/// [`Flag::takes_value`] has always claimed and what nothing implemented until
/// task 95's verbs became the first in the registry to declare a flag at all:
/// before that the claim was unreachable, and the test that looked like it
/// checked both spellings only ever built the `=` one.
///
/// Done here rather than in [`tokenize`] because only the [`Verb`] knows which
/// flags take values — a tokenizer that guessed would swallow the positional
/// after a switch. `skip` is how many *words* the verb path used, so the words
/// counted past it are the ones left over for arguments.
///
/// Two limits are worth naming, both consequences of resolving the verb before
/// any flag's arity is known — which is the only order available, since it is the
/// verb that declares its flags.
///
/// `tag rules --mode set` resolves as the longer verb `tag rules set` with a
/// valueless `--mode`, and reports that: longest-prefix is the rule everywhere
/// else in this grammar, and making flags an exception would mean the verb a line
/// names depended on which flags it carried.
///
/// A *space-separated* value ahead of the verb path (`tag --sync imap new`) is
/// indistinguishable from another path segment and fails as an unknown verb. The
/// `=` form carries its value inside one token and works anywhere. vim's `:` has
/// the same shape — the command comes first.
fn bind(
    verb: &Verb,
    tokens: &[Token],
    skip: usize,
) -> Result<(Vec<ParsedFlag>, Vec<String>), CommandError> {
    let mut flags: Vec<ParsedFlag> = Vec::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut seen_words = 0;
    let mut awaiting: Option<String> = None;
    for token in tokens {
        match token {
            Token::Word(word) => {
                // A word owed to a flag is that flag's value wherever it sits,
                // including inside the span the verb path was counted from —
                // `tag --mode=x new` and `tag --mode x new` have to agree.
                if let Some(name) = awaiting.take() {
                    flags.push(ParsedFlag {
                        name,
                        value: Some(word.clone()),
                    });
                    continue;
                }
                seen_words += 1;
                if seen_words > skip {
                    positionals.push(word.clone());
                }
            }
            Token::Flag { name, value } => {
                if let Some(name) = awaiting.take() {
                    // A value-taking flag followed by another flag: nothing to
                    // pair it with, and `check_flags` is what says so by name.
                    flags.push(ParsedFlag { name, value: None });
                }
                let declared = verb.flags.iter().find(|flag| flag.name == *name);
                match declared {
                    Some(declared) if declared.takes_value && value.is_none() => {
                        awaiting = Some(name.clone());
                    }
                    _ => flags.push(ParsedFlag {
                        name: name.clone(),
                        value: value.clone(),
                    }),
                }
            }
        }
    }
    if let Some(name) = awaiting {
        flags.push(ParsedFlag { name, value: None });
    }
    Ok((flags, positionals))
}

fn check_flags(verb: &Verb, flags: &[ParsedFlag]) -> Result<(), CommandError> {
    for flag in flags {
        let Some(declared) = verb.flags.iter().find(|f| f.name == flag.name) else {
            return Err(CommandError::UnknownFlag {
                verb: verb.canonical(),
                flag: flag.name.clone(),
                valid: verb.flags.iter().map(|f| format!("--{}", f.name)).collect(),
            });
        };
        if declared.takes_value && flag.value.is_none() {
            return Err(CommandError::MissingFlagValue {
                flag: flag.name.clone(),
            });
        }
    }
    Ok(())
}

fn check_positionals(verb: &Verb, positionals: &[String]) -> Result<(), CommandError> {
    for (idx, declared) in verb.positionals.iter().enumerate() {
        if declared.required && positionals.get(idx).is_none() {
            return Err(CommandError::MissingPositional {
                verb: verb.canonical(),
                name: declared.name,
            });
        }
    }
    Ok(())
}

/// The registry's closest canonical path to `attempted`, ranked the way
/// `overlays::command_matches` (task 85) ranks the command palette — reusing
/// "is this roughly what was meant" rather than this module inventing a
/// second notion of fuzzy closeness: a prefix match beats a substring match
/// beats "every character of `attempted`, in order, somewhere in the
/// candidate," checked in that one direction only. The reverse direction
/// (is the *candidate* a subsequence of what was typed) looks like it
/// would make closeness usefully symmetric, but does the opposite: it lets
/// a short, unrelated verb like `search` "match" a garbled long one
/// (`message archiv`, missing only its final `e`, would suggest `search`
/// instead of `message archive`) — a short word's letters are easy to find
/// scattered through almost any longer typo, which is exactly backwards
/// from what a suggestion should optimize for. `None` when nothing is
/// close enough to be worth naming over just listing every candidate (a
/// task 89 caller's fallback).
fn closest(attempted: &str) -> Option<String> {
    let needle = attempted.to_ascii_lowercase();
    let mut scored: Vec<(u8, String)> = Vec::new();
    for verb in registry() {
        let candidate = verb.canonical();
        let candidate_lower = candidate.to_ascii_lowercase();
        let tier = if candidate_lower.starts_with(&needle) {
            0
        } else if candidate_lower.contains(&needle) {
            1
        } else if is_subsequence(&needle, &candidate_lower) {
            2
        } else {
            continue;
        };
        scored.push((tier, candidate));
    }
    scored
        .into_iter()
        .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.len().cmp(&b.1.len())))
        .map(|(_, candidate)| candidate)
}

/// Whether every character of `needle`, in order, appears somewhere in
/// `haystack`.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut haystack = haystack.chars();
    needle.chars().all(|c| haystack.by_ref().any(|h| h == c))
}

// ---------------------------------------------------------------------------
// complete
// ---------------------------------------------------------------------------

/// One completion candidate: what the command line's WhichKey band (task 91)
/// renders as a thing that could be typed next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The text this candidate would insert.
    pub text: String,
    /// Whether choosing this candidate still leaves more to type (`true`
    /// for a verb-path segment with children of its own, `false` for a
    /// leaf verb or a flag) — `command_band` reads this the same way
    /// `rmail_core::keymap::Continuation::leads` distinguishes `Leads::Run`
    /// from `Leads::Group` for a chord, so one `Kind` enum on the band side
    /// drives both without either source needing to imitate the other's
    /// shape. `complete`'s own alphabetical order is kept rather than
    /// re-sorted leaves-first: unlike a chord prefix (usually one or two
    /// live continuations), a verb-path segment can have a dozen-plus
    /// children, and sorting every group behind every leaf buries exactly
    /// the namespaces worth navigating toward.
    pub has_more: bool,
}

/// Every candidate for what comes next after `text`, positionally: the
/// verb registry while a path is still being typed, then that verb's own
/// flags once the path resolves — module docs' completion table.
/// Positional *values* (a folder name, a tag) are not this module's job;
/// it has no daemon connection and no model state to offer them from,
/// which is task 89's `Model`-backed completion, layered on top of this
/// for the columns this function cannot fill in.
#[must_use]
pub fn complete(text: &str) -> Vec<Candidate> {
    let Ok((_, rest)) = strip_range(text) else {
        return Vec::new();
    };
    let Ok(tokens) = tokenize(rest) else {
        return Vec::new();
    };
    let words: Vec<String> = tokens
        .iter()
        .filter_map(|t| match t {
            Token::Word(w) => Some(w.clone()),
            Token::Flag { .. } => None,
        })
        .collect();

    // A trailing `.` finishes a segment exactly the way a trailing space
    // does (module docs' "one transform") — `complete("message.")` has to
    // offer `message`'s children, not re-suggest `message` itself, which a
    // check for trailing *whitespace* alone would do (the word `tokenize`
    // hands back is literally `"message."`, dot included — `tokenize`
    // itself is deliberately not `.`-aware; see its own docs).
    let ends_with_separator =
        rest.ends_with(char::is_whitespace) || rest.ends_with('.') || rest.is_empty();

    // Every settled word is dot-expanded the same way `parse_verb` matches
    // a verb path — one glued word can name several segments. The
    // in-progress last word, when the line has not just ended a segment,
    // gets the same treatment for everything but its own final piece,
    // which is the partial filter still being typed.
    let mut prefix: Vec<&str> = Vec::new();
    let partial: &str = if ends_with_separator {
        for word in &words {
            prefix.extend(split_path(word));
        }
        ""
    } else {
        match words.split_last() {
            Some((last, settled)) => {
                for word in settled {
                    prefix.extend(split_path(word));
                }
                let mut last_segments = split_path(last);
                let partial = last_segments.pop().unwrap_or("");
                prefix.extend(last_segments);
                partial
            }
            None => "",
        }
    };

    let children = children_of(&prefix);
    let mut seen_next_segments = BTreeSet::new();
    let mut out = Vec::new();
    for verb in &children {
        let next = verb.path[prefix.len()];
        if !next.starts_with(partial) || !seen_next_segments.insert(next) {
            continue;
        }
        // Whether *any* verb sharing this next segment goes deeper, not
        // just the one that happened to be first: `search` and
        // `search.explain` both real, both auto-derived, and registry
        // order (`Action::ALL`'s) puts the leaf first — computing this
        // from `verb` alone would report `search` as childless.
        let has_more = children
            .iter()
            .any(|v| v.path[prefix.len()] == next && v.path.len() > prefix.len() + 1);
        out.push(Candidate {
            text: next.to_owned(),
            has_more,
        });
    }
    // An exact verb at `prefix` with no completed word yet offers its own
    // flags — `:tag add ` (trailing space) should suggest `--sync`, not
    // repeat `add`.
    if partial.is_empty() {
        if let Some(verb) = verb_at(&prefix) {
            out.extend(flag_candidates(verb));
        }
    }
    out.sort_by(|a, b| a.text.cmp(&b.text));
    out
}

/// The `--flag` candidates [`complete`] offers once a verb's path is fully
/// typed with nothing yet started for the next word — pulled out of
/// [`complete`] so it can be tested directly the same way
/// [`check_flags`]/[`check_positionals`] are: a [`Verb`] cannot be
/// registered into the process-wide registry from a test, and no real verb
/// declares any flags yet ([`explicit`]'s docs).
fn flag_candidates(verb: &Verb) -> Vec<Candidate> {
    verb.flags
        .iter()
        .map(|flag| Candidate {
            text: format!("--{}", flag.name),
            has_more: flag.takes_value,
        })
        .collect()
}
