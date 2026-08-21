//! Notes, saved searches and smart folders — the CRUD the acceptance names, and
//! the distinction between the last two.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_proto::v1::{
    ListNotesResponse, ListSavedSearchesResponse, ListSmartFoldersResponse, Note, SavedSearch,
    SmartFolder, SmartFolderEvaluation,
};

use super::super::tests::{loaded, no_account, no_message, run, screen};
use super::*;
use crate::tui::model::wire;
use crate::tui::model::Overlay;
use crate::tui::report::ReportTone;

fn invocation(line: &str) -> Invocation {
    match command::parse(line) {
        Ok(Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
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
        other => panic!("{line:?} is not a request: {other:?}"),
    }
}

fn refusal(line: &str, target: &Target) -> String {
    match asked(line, target) {
        Answer::Refused(why) => why,
        other => panic!("{line:?} was not refused: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// notes
// ---------------------------------------------------------------------------

#[test]
fn a_note_is_about_the_message_or_its_thread() {
    // There is no separate thread id to type: the TUI's notion of "this thread"
    // is the thread of the message under the cursor, and the daemon resolves it
    // from the message.
    assert_eq!(
        request("note add chased this").cmd,
        Cmd::NoteAdd {
            message_id: 10,
            thread: false,
            body: "chased this".to_owned(),
        }
    );
    let Cmd::NoteAdd { thread, .. } = request("note add chased this --thread").cmd else {
        panic!("expected a note");
    };
    assert!(thread);
    let Cmd::NoteList { thread, .. } = request("note list --thread").cmd else {
        panic!("expected a listing");
    };
    assert!(thread);
}

#[test]
fn a_note_needs_something_written_in_it() {
    assert!(refusal("note add", &screen()).contains("write something"));
    assert!(refusal("note add", &screen()).contains(":note add"));
    assert!(refusal("note add x", &no_message()).contains("no message selected"));
}

#[test]
fn editing_a_note_takes_its_id_and_the_rest_of_the_line() {
    // Splitting the body word by word would be the silent truncation `joined`'s
    // own docs call out, and `joined` alone would swallow the id.
    assert_eq!(
        request("note edit 4 chased again on Friday").cmd,
        Cmd::NoteEdit {
            note_id: 4,
            body: "chased again on Friday".to_owned(),
        }
    );
    assert!(refusal("note edit", &screen()).contains("which note"));
    assert!(refusal("note edit 4", &screen()).contains("what it should say"));
    assert!(refusal("note rm", &screen()).contains("which note"));
    assert_eq!(request("note rm 4").cmd, Cmd::NoteDelete { note_id: 4 });
}

#[test]
fn a_note_the_model_wrote_is_drawn_differently_from_one_you_wrote() {
    // A summary the model wrote and a decision you recorded are different claims,
    // and a listing that drew them identically would invite treating one as the
    // other.
    let response = ListNotesResponse {
        notes: vec![
            Note {
                id: 1,
                target: None,
                body_md: "chased this".to_owned(),
                author: 1,
                created_at: 1_700_000_000,
                updated_at: 0,
            },
            Note {
                id: 2,
                target: None,
                body_md: "summarised by the pipeline".to_owned(),
                author: 2,
                created_at: 1_700_000_100,
                updated_at: 0,
            },
        ],
    };
    let rows = wire::note_rows(&response);
    assert_eq!(rows[0].cells[1], "you");
    assert_eq!(rows[0].tone, ReportTone::Plain);
    assert_eq!(rows[1].cells[1], "ai");
    assert_eq!(rows[1].tone, ReportTone::Muted);
}

#[test]
fn a_deleted_note_arrives_as_a_row_rather_than_rewriting_the_list() {
    // The live pane appends, and rewriting history under a reader who is looking
    // at it is worse than saying what changed.
    use rmail_proto::v1::note_event::Event;
    let added = wire::note_event_row(&rmail_proto::v1::NoteEvent {
        event: Some(Event::Added(Note {
            id: 3,
            body_md: "new".to_owned(),
            author: 1,
            ..Default::default()
        })),
    })
    .expect("a row");
    assert_eq!(added.cells[0], "3");
    let deleted = wire::note_event_row(&rmail_proto::v1::NoteEvent {
        event: Some(Event::Deleted(rmail_proto::v1::DeletedNote {
            id: 3,
            target: None,
        })),
    })
    .expect("a row");
    assert_eq!(deleted.cells[3], "deleted");
    assert_eq!(deleted.tone, ReportTone::Bad);
}

// ---------------------------------------------------------------------------
// saved searches
// ---------------------------------------------------------------------------

#[test]
fn saving_and_editing_are_two_verbs_rather_than_an_upsert() {
    // `Create` refuses a name that exists and `Update` refuses one that does not.
    // An upsert would quietly store a typo'd name as a new entry.
    assert_eq!(
        request("saved save unpaid from:stripe is:unread").cmd,
        Cmd::SavedSet {
            account_id: 7,
            name: "unpaid".to_owned(),
            query: "from:stripe is:unread".to_owned(),
            update: false,
        }
    );
    let Cmd::SavedSet { update, .. } = request("saved edit unpaid from:stripe").cmd else {
        panic!("expected a store");
    };
    assert!(update);
    assert!(refusal("saved save", &screen()).contains("name it"));
    assert!(refusal("saved save unpaid", &screen()).contains("the query it stands for"));
}

#[test]
fn a_saved_search_row_runs_it() {
    let response = ListSavedSearchesResponse {
        searches: vec![SavedSearch {
            id: 1,
            account_id: 7,
            name: "unpaid".to_owned(),
            query: "from:stripe".to_owned(),
            created_at: 0,
            updated_at: 0,
            last_run_at: 1_700_000_000,
        }],
    };
    let rows = wire::saved_rows(&response);
    let runs = rows[0].on_enter.clone().expect("a row runs it");
    assert_eq!(runs.verb, vec!["saved", "run"]);
    assert_eq!(runs.positionals, vec!["unpaid".to_owned()]);
    assert!(runs.bang, "running a saved search is a read");
}

#[test]
fn a_name_with_a_space_in_it_survives_being_put_on_a_row() {
    // A name is whatever somebody typed; unquoted on a rebuilt line it would
    // split into two tokens and ask the verb about something else.
    let response = ListSavedSearchesResponse {
        searches: vec![SavedSearch {
            name: "unpaid invoices".to_owned(),
            ..Default::default()
        }],
    };
    let rows = wire::saved_rows(&response);
    let runs = rows[0].on_enter.clone().expect("a row runs it");
    assert_eq!(runs.positionals, vec!["unpaid invoices".to_owned()]);
}

#[test]
fn running_and_forgetting_a_saved_search_need_its_name() {
    assert_eq!(
        request("saved run unpaid --limit=5 --explain").cmd,
        Cmd::SavedRun {
            generation: 5,
            account_id: 7,
            name: "unpaid".to_owned(),
            limit: Some(5),
            explain: true,
        }
    );
    assert!(refusal("saved run", &screen()).contains("which one"));
    assert_eq!(
        request("saved rm unpaid").cmd,
        Cmd::SavedDelete {
            account_id: 7,
            name: "unpaid".to_owned(),
        }
    );
    assert!(refusal("saved rm", &screen()).contains("which one"));
}

// ---------------------------------------------------------------------------
// smart folders
// ---------------------------------------------------------------------------

#[test]
fn a_predicate_and_a_sentence_are_two_verbs() {
    // One spends money at a provider and the other does not, and that is not a
    // difference to hide behind whether a flag was given.
    let Cmd::FolderCreate { compile, text, .. } =
        request("folder new unpaid from:stripe is:unread").cmd
    else {
        panic!("expected a folder");
    };
    assert!(!compile);
    assert_eq!(text, "from:stripe is:unread");
    let Cmd::FolderCreate { compile, text, .. } =
        request("folder compile unpaid everything stripe has not been paid for").cmd
    else {
        panic!("expected a folder");
    };
    assert!(compile);
    assert_eq!(text, "everything stripe has not been paid for");
    // And they say different things about what is missing.
    assert!(refusal("folder new unpaid", &screen()).contains("predicate"));
    assert!(refusal("folder compile unpaid", &screen()).contains("in words"));
}

#[test]
fn a_folder_that_tags_what_enters_it_is_drawn_as_a_warning() {
    // It changes mail on its own, which is the one thing about a folder listing
    // worth spotting.
    let response = ListSmartFoldersResponse {
        folders: vec![
            SmartFolder {
                name: "unpaid".to_owned(),
                predicate: "from:stripe".to_owned(),
                auto_tag: "billing".to_owned(),
                ..Default::default()
            },
            SmartFolder {
                name: "quiet".to_owned(),
                predicate: "is:read".to_owned(),
                ..Default::default()
            },
        ],
    };
    let rows = wire::smart_folder_rows(&response);
    assert_eq!(rows[0].tone, ReportTone::Warn);
    assert_eq!(rows[0].cells[2], "billing");
    assert_eq!(rows[1].tone, ReportTone::Plain);
    assert_eq!(rows[1].cells[2], "-");
    // Enter lists what is in it, which is the question a folder listing raises.
    let members = rows[0].on_enter.clone().expect("a row lists members");
    assert_eq!(members.verb, vec!["folder", "members"]);
    assert_eq!(members.positionals, vec!["unpaid".to_owned()]);
}

#[test]
fn evaluating_a_folder_says_how_much_mail_it_changed() {
    let rows = wire::evaluation_rows(&SmartFolderEvaluation {
        smart_folder_id: 1,
        members: 40,
        entered: vec![1, 2],
        departed: vec![3],
        tagged: 2,
        notified: 0,
        entered_count: 2,
        departed_count: 1,
    });
    let cell = |what: &str| {
        rows.iter()
            .find(|row| row.cells[0] == what)
            .cloned()
            .unwrap_or_else(|| panic!("no {what} row"))
    };
    assert_eq!(cell("members").cells[1], "40");
    assert_eq!(cell("entered").cells[1], "2");
    // The row that says your mail was changed.
    assert_eq!(cell("tagged").tone, ReportTone::Warn);
    let quiet = wire::evaluation_rows(&SmartFolderEvaluation {
        tagged: 0,
        ..Default::default()
    });
    assert_eq!(
        quiet
            .iter()
            .find(|row| row.cells[0] == "tagged")
            .map(|row| row.tone),
        Some(ReportTone::Plain)
    );
}

#[test]
fn a_compiled_folder_shows_the_sentence_it_came_from_and_the_plan() {
    let folder = SmartFolder {
        name: "unpaid".to_owned(),
        predicate: "from:stripe is:unread".to_owned(),
        auto_tag: "billing".to_owned(),
        notify: true,
        nl_source: "everything stripe has not been paid for".to_owned(),
        ..Default::default()
    };
    let plan = rmail_proto::v1::QueryPlan {
        filters: vec!["from:stripe".to_owned()],
        semantic_query: "unpaid".to_owned(),
        ..Default::default()
    };
    let rows = wire::smart_folder_fields(&folder, Some(&plan));
    let cell = |what: &str| {
        rows.iter()
            .find(|row| row.cells[0] == what)
            .map(|row| row.cells[1].clone())
            .unwrap_or_else(|| panic!("no {what} row"))
    };
    assert_eq!(cell("predicate"), "from:stripe is:unread");
    assert_eq!(
        cell("compiled from"),
        "everything stripe has not been paid for"
    );
    assert_eq!(cell("filter"), "from:stripe");
    assert_eq!(cell("semantic"), "unpaid");
    // A hand-written folder has no sentence behind it, and no row claiming one.
    let rows = wire::smart_folder_fields(
        &SmartFolder {
            predicate: "is:read".to_owned(),
            ..Default::default()
        },
        None,
    );
    assert!(rows.iter().all(|row| row.cells[0] != "compiled from"));
}

#[test]
fn every_library_verb_needs_the_account_on_screen() {
    for line in [
        "saved list",
        "saved save x y",
        "saved run x",
        "saved rm x",
        "folder list",
        "folder new x y",
        "folder members x",
        "folder eval x",
        "folder rm x",
    ] {
        match asked(line, &no_account()) {
            Answer::Refused(why) => assert!(why.contains("no account"), "{line}: {why}"),
            other => panic!("{line}: {other:?}"),
        }
    }
}

#[test]
fn none_of_these_verbs_asks_before_it_runs() {
    // Every one of them is reversible or is itself the undo: a forgotten saved
    // search can be saved again, and a deleted note is one note. Task 89's rule
    // that a typed `:` line is already the deliberate act covers the rest.
    for line in [
        "note add x",
        "note edit 4 x",
        "note rm 4",
        "saved save x y",
        "saved rm x",
        "folder new x y",
        "folder eval x",
        "folder rm x",
    ] {
        assert!(request(line).confirm.is_none(), "{line} asks");
    }
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

#[test]
fn a_library_listing_opens_a_report_and_a_store_does_not() {
    let mut model = loaded();
    let cmds = run(&mut model, "saved list");
    assert!(
        matches!(cmds.first(), Some(Cmd::SavedList { .. })),
        "{cmds:?}"
    );
    assert!(matches!(model.overlay_top(), Some(Overlay::Report(_))));

    let mut model = loaded();
    let cmds = run(&mut model, "saved save unpaid from:stripe");
    assert!(
        matches!(cmds.first(), Some(Cmd::SavedSet { .. })),
        "{cmds:?}"
    );
    assert!(!model.overlay_is_open());
}
