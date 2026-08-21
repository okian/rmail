//! Task 95's tag verbs: the range that is the selection, the two forms that look
//! interchangeable and are not, and the suggestion rows that answer both ways.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_core::keymap::Key;

use super::*;
use crate::tui::model::{update, Account, Folder, MessageRow, Model, Msg, Overlay};
use crate::tui::report::ReportRow;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn invocation(line: &str) -> Invocation {
    match command::parse(line) {
        Ok(Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
    }
}

/// A screen with an account, an open folder and three messages selected.
fn screen() -> Target {
    Target {
        account_id: 7,
        mailbox_id: Some(1),
        message_id: Some(10),
        selection: vec![10, 11, 12],
        rule_draft: None,
    }
}

fn empty() -> Target {
    Target {
        account_id: 0,
        mailbox_id: None,
        message_id: None,
        selection: Vec::new(),
        rule_draft: None,
    }
}

fn asked(line: &str, target: &Target) -> Answer {
    match answer(&invocation(line), target, 5) {
        Some(answer) => answer,
        None => panic!("{line:?} has no answer"),
    }
}

fn request(line: &str) -> Request {
    match asked(line, &screen()) {
        Answer::Rows(request) | Answer::Fact(request) => *request,
        other => panic!("{line:?} was refused: {other:?}"),
    }
}

fn refusal(line: &str, target: &Target) -> String {
    match asked(line, target) {
        Answer::Refused(why) => why,
        other => panic!("{line:?} was not refused: {other:?}"),
    }
}

fn loaded() -> Model {
    let mut model = Model::new();
    model.account = Some(Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    });
    model.folders = vec![Folder {
        id: 1,
        name: "INBOX".to_owned(),
        message_count: 3,
    }];
    model.open_folder = Some(1);
    model.messages = (10..13)
        .map(|id| MessageRow {
            id,
            subject: format!("subject {id}"),
            from: "Alice".to_owned(),
            from_addr: Some("alice@example.com".to_owned()),
            date: Some(1_700_000_000 + id),
            flags: Vec::new(),
            has_attachments: false,
        })
        .collect();
    model
}

fn run(model: &mut Model, line: &str) -> Vec<Cmd> {
    update(model, Msg::Key(Key::Char(':')));
    for c in line.chars() {
        update(model, Msg::Key(Key::Char(c)));
    }
    update(model, Msg::Key(Key::Enter))
        .into_iter()
        .filter(|cmd| !matches!(cmd, Cmd::SaveHistory { .. }))
        .collect()
}

// ---------------------------------------------------------------------------
// the range is the selection
// ---------------------------------------------------------------------------

#[test]
fn tag_add_acts_on_the_whole_selection() {
    assert_eq!(
        request("tag add invoices").cmd,
        Cmd::TagApply {
            generation: 5,
            message_ids: vec![10, 11, 12],
            name: "invoices".to_owned(),
            remove: false,
        }
    );
}

#[test]
fn tag_rm_is_the_same_request_the_other_way() {
    let Cmd::TagApply { remove, .. } = request("tag rm invoices").cmd else {
        panic!("expected a tag application");
    };
    assert!(
        remove,
        "one request, one direction flag — not two code paths"
    );
}

#[test]
fn a_ranged_tag_add_is_the_same_set_the_key_acts_on() {
    // The claim task 89 made and this verb depends on: `'<,'>` needs no code of
    // its own, because `Target::selection` *is* `model::targets`' answer. Driven
    // through `update` so the selection is a real one.
    let mut model = loaded();
    update(&mut model, Msg::Key(Key::Char('v')));
    update(&mut model, Msg::Key(Key::Char('j')));
    // `:` prefills the range itself when a selection is up (task 89), so typing
    // it again would produce `'<,'>'<,'>tag …` — which is what a first draft of
    // this test did.
    let cmds = run(&mut model, "tag add invoices");
    assert!(model.pending.is_empty(), "nothing half-typed is left over");
    match cmds.first() {
        Some(Cmd::TagApply { message_ids, .. }) => assert_eq!(
            message_ids,
            &[10, 11],
            "the two rows the selection covers, and no others"
        ),
        other => panic!("expected a tag application, found {other:?}"),
    }
}

#[test]
fn tag_add_refuses_with_nothing_selected_and_with_no_tag_named() {
    assert!(refusal("tag add invoices", &empty()).contains("message"));
    assert!(refusal("tag add", &screen()).contains("tag"));
}

#[test]
fn a_tag_application_reports_a_row_per_message() {
    // A count would hide the outcome worth seeing: four applied, one failed.
    let columns = request("tag add invoices").columns;
    assert_eq!(columns.len(), 3);
    assert_eq!(columns[0].header, "message");
}

// ---------------------------------------------------------------------------
// bulk is not a ranged add
// ---------------------------------------------------------------------------

#[test]
fn tag_bulk_takes_a_query_and_never_the_selection() {
    assert_eq!(
        request("tag bulk \"from:stripe is:unread\" invoices").cmd,
        Cmd::TagBulk {
            generation: 5,
            account_id: 7,
            query: "from:stripe is:unread".to_owned(),
            name: "invoices".to_owned(),
        }
    );
}

#[test]
fn tag_bulk_refuses_without_both_a_query_and_a_tag() {
    let why = refusal("tag bulk invoices", &screen());
    assert!(why.contains("query"), "{why}");
}

#[test]
fn tag_bulk_needs_an_account_and_a_ranged_add_does_not() {
    // The asymmetry `parity`'s own note on `TagBulkTag` records: the bulk form
    // needs an account the per-message form does not.
    assert!(refusal("tag bulk \"from:x\" y", &empty()).contains("account"));
    let with_selection = Target {
        selection: vec![10],
        ..empty()
    };
    assert!(matches!(
        asked("tag add invoices", &with_selection),
        Answer::Rows(_)
    ));
}

// ---------------------------------------------------------------------------
// tags, and creating them
// ---------------------------------------------------------------------------

#[test]
fn tag_list_needs_an_account() {
    assert_eq!(
        request("tag list").cmd,
        Cmd::TagList {
            generation: 5,
            account_id: 7,
        }
    );
    assert!(refusal("tag list", &empty()).contains("account"));
}

#[test]
fn tag_new_carries_its_optional_shape() {
    assert_eq!(
        request("tag new invoices").cmd,
        Cmd::TagCreate {
            account_id: 7,
            name: "invoices".to_owned(),
            color: None,
            sync: None,
        }
    );
    assert_eq!(
        request("tag new invoices --color blue --sync imap").cmd,
        Cmd::TagCreate {
            account_id: 7,
            name: "invoices".to_owned(),
            color: Some("blue".to_owned()),
            sync: Some(Sync::Imap),
        }
    );
}

#[test]
fn a_sync_mode_that_names_nothing_is_refused_where_it_was_typed() {
    let why = refusal("tag new invoices --sync everywhere", &screen());
    assert!(why.contains("local"), "{why}");
    assert!(
        why.contains("everywhere"),
        "and it quotes what was typed: {why}"
    );
}

// ---------------------------------------------------------------------------
// suggestions, and answering them both ways
// ---------------------------------------------------------------------------

#[test]
fn tag_suggest_needs_a_message() {
    assert_eq!(
        request("tag suggest").cmd,
        Cmd::TagSuggest {
            generation: 5,
            message_id: 10,
        }
    );
    assert!(refusal("tag suggest", &empty()).contains("message"));
}

#[test]
fn accept_and_reject_are_one_rpc_read_in_two_directions() {
    assert_eq!(
        request("tag accept 42").cmd,
        Cmd::TagResolve {
            message_tag_id: 42,
            resolve: Resolve::Accept,
        }
    );
    assert_eq!(
        request("tag reject 42").cmd,
        Cmd::TagResolve {
            message_tag_id: 42,
            resolve: Resolve::Reject,
        }
    );
    assert!(Resolve::Accept.accept());
    assert!(!Resolve::Reject.accept());
}

#[test]
fn a_suggestion_id_that_is_not_a_number_is_refused() {
    let why = refusal("tag accept soon", &screen());
    assert!(why.contains("id"), "{why}");
}

#[test]
fn a_suggestion_row_carries_both_answers() {
    let row = crate::tui::model::wire::tag_suggestion_row(&rmail_proto::v1::TagSuggestion {
        message_tag_id: 42,
        tag: Some(rmail_proto::v1::Tag {
            name: "newsletters".to_owned(),
            ..rmail_proto::v1::Tag::default()
        }),
        confidence: 0.91,
        rationale: "a mailing-list footer".to_owned(),
    });
    assert_eq!(
        row.on_enter.as_ref().map(|i| i.verb.join(" ")),
        Some("tag accept".to_owned())
    );
    assert_eq!(
        row.on_reject.as_ref().map(|i| i.verb.join(" ")),
        Some("tag reject".to_owned()),
        "a list where accepting is inline and rejecting is not makes the safe \
         answer the awkward one"
    );
    assert_eq!(
        row.on_reject.as_ref().map(|i| i.positionals.clone()),
        Some(vec!["42".to_owned()]),
        "and both address the suggestion the row is about"
    );
}

#[test]
fn n_runs_the_highlighted_rows_rejection() {
    let mut model = loaded();
    let cmds = run(&mut model, "tag suggest");
    let generation = match cmds.first() {
        Some(Cmd::TagSuggest { generation, .. }) => *generation,
        other => panic!("expected a suggest, found {other:?}"),
    };
    let row = crate::tui::model::wire::tag_suggestion_row(&rmail_proto::v1::TagSuggestion {
        message_tag_id: 42,
        tag: Some(rmail_proto::v1::Tag {
            name: "newsletters".to_owned(),
            ..rmail_proto::v1::Tag::default()
        }),
        confidence: 0.91,
        rationale: String::new(),
    });
    update(
        &mut model,
        Msg::Report {
            generation,
            event: crate::tui::model::ReportEvent::Frame {
                fill: crate::tui::report::ReportFill::Append,
                rows: vec![row],
                complete: true,
            },
        },
    );
    let cmds = update(&mut model, Msg::Key(Key::Char('n')));
    assert_eq!(
        cmds,
        vec![Cmd::TagResolve {
            message_tag_id: 42,
            resolve: Resolve::Reject,
        }]
    );
    assert!(
        matches!(model.overlay_top(), Some(Overlay::Report(_))),
        "and the list stays up, because the next row is answered next"
    );
}

#[test]
fn n_on_a_row_with_nothing_to_reject_does_nothing() {
    // `n` is bound in the whole `Menu` layer and most rows there have no `no`.
    let mut model = loaded();
    let cmds = run(&mut model, "tag list");
    let generation = match cmds.first() {
        Some(Cmd::TagList { generation, .. }) => *generation,
        other => panic!("expected a tag listing, found {other:?}"),
    };
    update(
        &mut model,
        Msg::Report {
            generation,
            event: crate::tui::model::ReportEvent::Frame {
                fill: crate::tui::report::ReportFill::Replace,
                rows: vec![ReportRow::new(["invoices", "4"])],
                complete: true,
            },
        },
    );
    let cmds = update(&mut model, Msg::Key(Key::Char('n')));
    assert!(cmds.is_empty());
    assert!(matches!(model.overlay_top(), Some(Overlay::Report(_))));
}

// ---------------------------------------------------------------------------
// the rules that let a suggestion apply itself
// ---------------------------------------------------------------------------

#[test]
fn tag_rules_lists_and_tag_rules_set_writes() {
    assert_eq!(
        request("tag rules").cmd,
        Cmd::TagRules {
            generation: 5,
            account_id: 7,
        }
    );
    assert_eq!(
        request("tag rules set newsletters news").cmd,
        Cmd::TagRuleSet {
            account_id: 7,
            name: "newsletters".to_owned(),
            tag: "news".to_owned(),
            mode: RuleMode::Suggest,
            min_conf_pct: 90,
            enabled: true,
        },
        "suggest and 0.9 when nothing says otherwise — the same defaults \
         `mail tag-rules set` has"
    );
}

#[test]
fn the_default_mode_is_the_safe_half_of_the_pair() {
    // Without a rule at `auto`, every suggestion waits to be accepted. The
    // proto's own docs call that the safe default rather than an oversight, and
    // a TUI defaulting the other way would be the surface that quietly stopped
    // asking.
    let Cmd::TagRuleSet { mode, .. } = request("tag rules set a b").cmd else {
        panic!("expected a rule");
    };
    assert_eq!(mode, RuleMode::Suggest);
    let Cmd::TagRuleSet { mode, .. } = request("tag rules set a b --mode auto").cmd else {
        panic!("expected a rule");
    };
    assert_eq!(mode, RuleMode::Auto);
}

#[test]
fn a_mode_or_a_confidence_that_makes_no_sense_is_refused_rather_than_clamped() {
    // Clamping `--min-conf 5` to 1.0 would store a rule nobody asked for; a
    // rule at the wrong threshold is one that mis-tags mail without anybody
    // looking.
    assert!(refusal("tag rules set a b --mode always", &screen()).contains("suggest"));
    for bad in ["5", "-1", "soon"] {
        let why = refusal(&format!("tag rules set a b --min-conf {bad}"), &screen());
        assert!(why.contains("between 0 and 1"), "{bad}: {why}");
    }
}

#[test]
fn a_confidence_is_carried_as_whole_percent() {
    let Cmd::TagRuleSet { min_conf_pct, .. } = request("tag rules set a b --min-conf 0.95").cmd
    else {
        panic!("expected a rule");
    };
    assert_eq!(min_conf_pct, 95);
    // The bounds, so the conversion cannot produce something outside them.
    for (given, expected) in [("0", 0), ("1", 100), ("0.004", 0), ("0.996", 100)] {
        let Cmd::TagRuleSet { min_conf_pct, .. } =
            request(&format!("tag rules set a b --min-conf {given}")).cmd
        else {
            panic!("expected a rule");
        };
        assert_eq!(min_conf_pct, expected, "{given}");
    }
}

#[test]
fn disabled_stores_the_rule_retired_rather_than_deleting_it() {
    let Cmd::TagRuleSet { enabled, .. } = request("tag rules set a b --disabled").cmd else {
        panic!("expected a rule");
    };
    assert!(
        !enabled,
        "`SetTagRule` has a retired state and no delete, so this is the whole \
         vocabulary"
    );
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

#[test]
fn a_tag_table_opens_a_report_and_a_tag_fact_does_not() {
    let mut model = loaded();
    let cmds = run(&mut model, "tag list");
    assert!(
        matches!(cmds.first(), Some(Cmd::TagList { .. })),
        "{cmds:?}"
    );
    assert!(matches!(model.overlay_top(), Some(Overlay::Report(_))));

    let mut model = loaded();
    let cmds = run(&mut model, "tag new invoices");
    assert!(
        matches!(cmds.first(), Some(Cmd::TagCreate { .. })),
        "{cmds:?}"
    );
    assert!(
        !model.overlay_is_open(),
        "a one-line answer needs no screen"
    );
    assert_eq!(model.inflight, 1, "somebody asked for it");
}
