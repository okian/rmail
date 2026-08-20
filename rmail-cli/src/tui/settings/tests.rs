//! The settings screen. Every claim here holds with no daemon anywhere, which is
//! the property the screen was built for.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_core::keymap::{Key, Mode};

use super::*;
use crate::tui::commands;
use crate::tui::model::{update, Account, Cmd, Folder, MessageRow, Model, Msg, Overlay, Screen};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn loaded() -> Model {
    let mut model = Model::new();
    model.accounts = vec![Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    }];
    model.account = model.accounts.first().cloned();
    model.folders = vec![Folder {
        id: 1,
        name: "INBOX".to_owned(),
        message_count: 2,
    }];
    model.open_folder = Some(1);
    model.messages = (10..12)
        .map(|id| MessageRow {
            id,
            subject: format!("subject {id}"),
            from: "Alice".to_owned(),
            from_addr: Some("alice@example.com".to_owned()),
            date: Some(1_700_000_000 + id),
            flags: Vec::new(),
            has_attachments: true,
        })
        .collect();
    model
}

fn press(model: &mut Model, key: Key) -> Vec<Cmd> {
    update(model, Msg::Key(key))
        .into_iter()
        .filter(|cmd| !matches!(cmd, Cmd::SaveHistory { .. }))
        .collect()
}

fn open(model: &mut Model) {
    press(model, Key::Char('g'));
    press(model, Key::Char('s'));
}

fn state(model: &Model) -> &SettingsState {
    match model.settings.as_ref() {
        Some(settings) => settings,
        None => panic!("expected the settings screen"),
    }
}

/// Move to the field with this label, and say whether it was found.
fn go_to(model: &mut Model, label: &str) -> bool {
    let at = state(model)
        .fields
        .iter()
        .position(|field| field.label == label);
    match at {
        Some(at) => {
            if let Some(settings) = model.settings.as_mut() {
                settings.cursor = at;
            }
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// every line is a real one
// ---------------------------------------------------------------------------

#[test]
fn every_line_parses() {
    // The claim the whole screen rests on: a field's write is a `:` line
    // somebody could type. A line that does not parse would be a field that
    // opens, accepts a keypress, and then refuses — the worst of the three
    // outcomes.
    for section in Section::ALL {
        for field in section.fields() {
            for line in lines_of(&field) {
                let parsed = command::parse(line).unwrap_or_else(|error| {
                    panic!("{} › {}: {line:?}: {error}", section.id(), field.label)
                });
                assert!(
                    matches!(parsed, Resolution::Invocation(_)),
                    "{} › {}: {line:?} names an interior node, not a verb",
                    section.id(),
                    field.label
                );
            }
        }
    }
}

/// Every `:` line a field can produce.
fn lines_of(field: &Field) -> Vec<&'static str> {
    match &field.kind {
        FieldKind::Toggle(options) => options.iter().map(|option| option.line).collect(),
        FieldKind::Choice(options) => options.iter().map(|option| option.line).collect(),
        FieldKind::Number { line }
        | FieldKind::Run { line }
        | FieldKind::ReadOnly(ReadOnly::ConfigFileOnly { line }) => vec![line],
        // A `Text` field runs nothing — it puts the verb on the command line for
        // somebody to finish — so its line is deliberately *not* a complete one
        // and is checked separately by `every_text_field_names_a_real_verb`.
        FieldKind::Text { .. } | FieldKind::ReadOnly(ReadOnly::NoRpc { .. }) => Vec::new(),
    }
}

#[test]
fn every_text_field_names_a_real_verb() {
    // Its line is incomplete by design, so it cannot be parsed — but the *verb*
    // on it has to exist, or the screen would open the command line with
    // something nobody can finish.
    for section in Section::ALL {
        for field in section.fields() {
            let FieldKind::Text { line } = field.kind else {
                continue;
            };
            let path: Vec<&str> = line
                .trim_end_matches(|c: char| c == '=' || c.is_whitespace())
                .split_whitespace()
                .take_while(|word| !word.starts_with("--"))
                .collect();
            assert!(
                command::verb_at(&path).is_some() || !command::children_of(&path).is_empty(),
                "{} › {}: {line:?} names no verb",
                section.id(),
                field.label
            );
        }
    }
}

#[test]
fn every_line_is_answered_by_something() {
    // A field whose verb parses but that nothing answers would open a pane and
    // then sit there. Asked of the answer table directly rather than inferred
    // from the invocation's capability, because task 98's block verbs reach no
    // capability *and* are answered — `commands::answer` is the authority.
    //
    // The exceptions are the verbs `run_invocation` answers by hand: three that
    // an `Action` carries (`help`, `manual`) or that write a local file (`set`,
    // `keys set`), and four that navigate or read what is already in the model.
    let by_hand = [
        "set",
        "keys set",
        "help",
        "manual",
        "settings",
        "toml",
        "attach list",
        "message open",
    ];
    let target = commands::Target {
        account_id: 7,
        mailbox_id: Some(1),
        message_id: Some(10),
        selection: Vec::new(),
        rule_draft: None,
    };
    for section in Section::ALL {
        for field in section.fields() {
            for line in lines_of(&field) {
                let Ok(Resolution::Invocation(invocation)) = command::parse(line) else {
                    continue;
                };
                let verb = invocation.verb.join(" ");
                if by_hand.contains(&verb.as_str()) {
                    continue;
                }
                assert!(
                    commands::answer(&invocation, &target, 1).is_some(),
                    "{} › {}: nothing answers {verb}",
                    section.id(),
                    field.label
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the sections
// ---------------------------------------------------------------------------

#[test]
fn the_acceptances_fourteen_sections_are_all_here() {
    let names: Vec<&str> = Section::ALL.iter().map(|section| section.id()).collect();
    assert_eq!(
        names,
        [
            "accounts",
            "sync",
            "index",
            "ai",
            "safety",
            "rules",
            "tags",
            "automation",
            "notifications",
            "saved",
            "keys",
            "interface",
            "tokens",
            "daemon",
        ]
    );
}

#[test]
fn every_section_is_reachable_by_name_and_has_fields() {
    for section in Section::ALL {
        assert_eq!(Section::from_id(section.id()), Some(*section));
        assert!(
            !section.fields().is_empty(),
            "{} has no fields, so it is a page nobody can do anything on",
            section.id()
        );
    }
    assert_eq!(Section::from_id("nonsense"), None);
}

#[test]
fn tab_walks_every_section_and_wraps() {
    // Not a fixed list: `next` reads `ALL`, so this walks whatever is there and
    // asserts it comes back where it started having seen everything.
    let mut seen = Vec::new();
    let mut section = Section::Accounts;
    for _ in 0..Section::ALL.len() {
        seen.push(section);
        section = section.next();
    }
    assert_eq!(section, Section::Accounts, "it wraps");
    assert_eq!(seen.len(), Section::ALL.len(), "and sees each one once");
}

// ---------------------------------------------------------------------------
// what a keypress produces
// ---------------------------------------------------------------------------

#[test]
fn a_toggle_moves_to_the_other_state_and_runs_its_line() {
    let sync = Section::Sync.fields();
    let field = sync
        .iter()
        .find(|field| field.label == "fetching")
        .expect("Sync has a fetching toggle");
    // It starts on the first option and `<enter>` moves to the second — the
    // screen does not know which one is true, which is the module docs' point.
    assert_eq!(field.value(), "running");
    let Accepted::Run { line, at } = field.accept() else {
        panic!("a toggle runs something");
    };
    assert_eq!(line, "sync pause");
    assert_eq!(at, 1);
    // And back again, so a toggle is a toggle.
    let mut field = field.clone();
    field.at = at;
    assert_eq!(field.value(), "paused");
    let Accepted::Run { line, at } = field.accept() else {
        panic!("a toggle runs something");
    };
    assert_eq!(line, "sync resume");
    assert_eq!(at, 0);
}

#[test]
fn a_choice_cycles_through_every_option() {
    let ai = Section::Ai.fields();
    let field = ai
        .iter()
        .find(|field| field.label == "backend")
        .expect("AI has a backend choice");
    let mut field = field.clone();
    let mut lines = Vec::new();
    for _ in 0..field.options().len() {
        let Accepted::Run { line, at } = field.accept() else {
            panic!("a choice runs something");
        };
        lines.push(line);
        field.at = at;
    }
    assert_eq!(
        lines,
        [
            "ai provider set local",
            "ai provider set claude",
            "ai provider set clear",
        ]
    );
    assert_eq!(field.at, 0, "and it comes back round");
}

#[test]
fn a_text_field_runs_nothing_and_asks_to_be_typed() {
    // The one kind with no write to express: an address, a token label, a chord
    // and a query are things only the person at the keyboard has.
    let accounts = Section::Accounts.fields();
    let field = accounts
        .iter()
        .find(|field| field.label == "add an account")
        .expect("Accounts has an add field");
    assert_eq!(
        field.accept(),
        Accepted::Type {
            line: "account add"
        }
    );
}

#[test]
fn a_config_only_field_runs_the_line_that_renders_its_block() {
    let notifications = Section::Notifications.fields();
    let field = notifications
        .iter()
        .find(|field| field.label == "threshold")
        .expect("Notifications has a threshold");
    assert!(matches!(
        field.kind,
        FieldKind::ReadOnly(ReadOnly::ConfigFileOnly { .. })
    ));
    let Accepted::Run { line, .. } = field.accept() else {
        panic!("it renders a block");
    };
    assert_eq!(line, "notify set --threshold=high");
    // And that line really does produce a block rather than a write, which is
    // what makes the field read-only rather than merely awkward.
    let target = commands::Target {
        account_id: 7,
        mailbox_id: Some(1),
        message_id: Some(10),
        selection: Vec::new(),
        rule_draft: None,
    };
    let invocation = invocation(line).expect("it parses");
    assert!(
        matches!(
            commands::answer(&invocation, &target, 1),
            Some(commands::Answer::Block(_))
        ),
        "a config-file-only field's line has to render a block"
    );
    assert_eq!(field.value(), "in the config file");
}

#[test]
fn a_no_rpc_field_says_what_would_have_to_exist() {
    let safety = Section::Safety.fields();
    let field = safety
        .iter()
        .find(|field| matches!(field.kind, FieldKind::ReadOnly(ReadOnly::NoRpc { .. })))
        .expect("Safety has one");
    let Accepted::Say { why } = field.accept() else {
        panic!("it explains itself");
    };
    assert!(!why.is_empty());
    assert_eq!(field.value(), why);
}

#[test]
fn every_field_that_runs_something_can_say_what() {
    // The acceptance's own property, over the whole screen: a keypress produces
    // an invocation, with no daemon anywhere.
    for section in Section::ALL {
        for field in section.fields() {
            match field.accept() {
                Accepted::Run { line, .. } => {
                    invocation(line).unwrap_or_else(|error| {
                        panic!("{} › {}: {line:?}: {error}", section.id(), field.label)
                    });
                }
                Accepted::Type { .. } | Accepted::Say { .. } => {}
                Accepted::Nothing => {
                    panic!("{} › {} does nothing at all", section.id(), field.label)
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the screen, end to end
// ---------------------------------------------------------------------------

#[test]
fn gs_opens_it_and_esc_closes_it() {
    let mut model = loaded();
    open(&mut model);
    assert_eq!(model.screen, Screen::Settings);
    assert_eq!(model.mode(), Mode::Settings);
    assert_eq!(state(&model).section, Section::Accounts);
    press(&mut model, Key::Esc);
    assert_eq!(model.screen, Screen::List);
    assert!(
        model.settings.is_none(),
        "the state goes with the screen, the way the manual's does"
    );
}

#[test]
fn the_screen_and_its_state_never_disagree() {
    // The same invariant `the_manual_state_and_the_screen_agree` holds for the
    // manual, and `set_screen` is the only place either is assigned.
    let mut model = loaded();
    open(&mut model);
    assert!(model.settings.is_some());
    // Leaving by any route drops it: `q` here, not just `<esc>`.
    press(&mut model, Key::Char('q'));
    assert_eq!(model.screen, Screen::List);
    assert!(model.settings.is_none());
}

#[test]
fn tab_moves_to_the_next_section_and_reopening_comes_back_to_it() {
    let mut model = loaded();
    open(&mut model);
    press(&mut model, Key::Tab);
    assert_eq!(state(&model).section, Section::Sync);
    assert_eq!(state(&model).cursor, 0, "a new section starts at the top");
    press(&mut model, Key::Esc);
    open(&mut model);
    assert_eq!(
        state(&model).section,
        Section::Sync,
        "reopening goes back where you were"
    );
}

#[test]
fn the_verb_opens_a_named_section_and_refuses_one_it_does_not_have() {
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    for c in "settings notifications".chars() {
        press(&mut model, Key::Char(c));
    }
    press(&mut model, Key::Enter);
    assert_eq!(state(&model).section, Section::Notifications);

    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    for c in "settings nonsense".chars() {
        press(&mut model, Key::Char(c));
    }
    press(&mut model, Key::Enter);
    // Left open with the offending word still there to fix, the way `:set`'s
    // complaint is.
    let why = match model.overlay.as_ref() {
        Some(Overlay::Command(pane)) => pane.error.clone().unwrap_or_default(),
        other => panic!("expected the command line to stay open: {other:?}"),
    };
    assert!(why.contains("accounts"), "{why}");
    assert_eq!(model.screen, Screen::List);
}

#[test]
fn enter_on_a_field_dispatches_the_line_and_records_it() {
    // Through `run_invocation`, which is the whole point: the field cannot skip
    // anything a typed line goes through, and what it did is visible afterwards
    // in the same history everything else typed is.
    let mut model = loaded();
    open(&mut model);
    press(&mut model, Key::Tab);
    assert!(go_to(&mut model, "sync now"));
    let cmds = press(&mut model, Key::Enter);
    assert!(
        matches!(cmds.first(), Some(Cmd::SyncNow { .. })),
        "{cmds:?}"
    );
    assert!(
        model
            .history
            .entries()
            .iter()
            .any(|line| line == "sync now"),
        "{:?}",
        model.history.entries()
    );
}

#[test]
fn a_field_cannot_skip_a_confirmation_its_verb_asks_for() {
    // `:index rebuild` asks `[y/N]`, and pressing `<enter>` on the field *is*
    // `:index rebuild` — so it asks too. A screen with a private path to the
    // daemon is exactly how that gets lost.
    let mut model = loaded();
    open(&mut model);
    for _ in 0..2 {
        press(&mut model, Key::Tab);
    }
    assert_eq!(state(&model).section, Section::Index);
    assert!(go_to(&mut model, "rebuild"));
    let cmds = press(&mut model, Key::Enter);
    assert!(
        cmds.is_empty(),
        "nothing runs until the question is answered"
    );
    assert!(
        matches!(model.overlay, Some(Overlay::Confirm { .. })),
        "{:?}",
        model.overlay
    );
}

#[test]
fn a_text_field_puts_the_verb_on_the_command_line() {
    let mut model = loaded();
    open(&mut model);
    assert!(go_to(&mut model, "add an account"));
    press(&mut model, Key::Enter);
    match model.overlay.as_ref() {
        Some(Overlay::Command(pane)) => assert_eq!(pane.input, "account add "),
        other => panic!("expected a prefilled command line: {other:?}"),
    }
}

#[test]
fn a_no_rpc_field_complains_rather_than_doing_nothing() {
    let mut model = loaded();
    open(&mut model);
    for _ in 0..4 {
        press(&mut model, Key::Tab);
    }
    assert_eq!(state(&model).section, Section::Safety);
    assert!(go_to(&mut model, "when a flag withholds actions"));
    let cmds = press(&mut model, Key::Enter);
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(
        model.status.contains("config file only"),
        "{}",
        model.status
    );
}

#[test]
fn the_keys_section_writes_no_request_at_all() {
    // The acceptance's own requirement: rebinding goes through
    // `rmail_core::keymap::file`, not `ConfigService.SetBinding`, because a
    // keymap you cannot fix with the daemon down is a keymap you are stuck with.
    let keys = Section::Keys.fields();
    for field in &keys {
        for line in lines_of(field) {
            let Ok(Resolution::Invocation(invocation)) = command::parse(line) else {
                continue;
            };
            assert!(
                invocation.capability.is_none(),
                "Keys › {}: {line:?} reaches {:?} over the wire",
                field.label,
                invocation.capability
            );
        }
    }
    // And the field that actually rebinds is the one that puts `keys set` on the
    // command line, which `run_invocation` answers by writing the file.
    let rebind = keys
        .iter()
        .find(|field| field.label == "rebind")
        .expect("Keys has a rebind field");
    assert_eq!(rebind.accept(), Accepted::Type { line: "keys set" });
}

#[test]
fn j_and_k_move_between_fields_and_stop_at_the_ends() {
    let mut model = loaded();
    open(&mut model);
    let last = state(&model).fields.len() - 1;
    press(&mut model, Key::Char('k'));
    assert_eq!(state(&model).cursor, 0, "up from the top stays put");
    for _ in 0..last + 4 {
        press(&mut model, Key::Char('j'));
    }
    assert_eq!(state(&model).cursor, last, "down past the end stays put");
    press(&mut model, Key::Char('g'));
    press(&mut model, Key::Char('g'));
    assert_eq!(state(&model).cursor, 0);
    press(&mut model, Key::Char('G'));
    assert_eq!(state(&model).cursor, last);
}

#[test]
fn the_settings_layer_does_not_reach_the_list_behind_it() {
    // `Mode::Settings`' chain stops at `Global`, so `a` — archive, in `Normal` —
    // is unbound here rather than acting on a message that is not on screen.
    let mut model = loaded();
    // Focused on the message list first, so `a` in `Normal` really would archive
    // something. Without this the test would pass under a broken layer for the
    // wrong reason — `archive` refuses with no message selected, which looks
    // exactly like the key being unbound.
    model.focus = crate::tui::model::Focus::Messages;
    open(&mut model);
    let cmds = press(&mut model, Key::Char('a'));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(model.screen, Screen::Settings, "and it is still up");
    assert!(
        model.status.is_empty() || !model.status.contains("archiv"),
        "`a` was answered by the list behind it: {}",
        model.status
    );
    let cmds = press(&mut model, Key::Char('d'));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(model.overlay.is_none(), "no delete question was asked");
}

#[test]
fn s_from_a_report_opens_it_and_takes_the_report_down() {
    // A table of what a subsystem is doing and the switches behind it are the
    // same subject, and the report's own stream goes with it — the rule `:` and
    // the manual follow when they open over a list overlay.
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    for c in "sync status".chars() {
        press(&mut model, Key::Char(c));
    }
    press(&mut model, Key::Enter);
    assert!(matches!(model.overlay, Some(Overlay::Report(_))));
    let cmds = press(&mut model, Key::Char('s'));
    assert!(model.overlay.is_none(), "the report went down");
    assert_eq!(model.screen, Screen::Settings);
    assert!(
        cmds.iter()
            .any(|cmd| matches!(cmd, Cmd::CancelStream { .. })),
        "and its stream was cancelled: {cmds:?}"
    );
}
