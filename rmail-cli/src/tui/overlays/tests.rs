//! Behaviour tests for task 85's overlays.
//!
//! These drive `tui::model::update` end to end — a key press in, commands and
//! state out — with no terminal, no daemon and no runtime, which is what lets
//! them cover the paths a hand-driven TUI never reaches: a stream frame that
//! arrives after its query was superseded, an answer the daemon refuses to
//! call grounded, an undo window that closes while the toast is up.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use super::*;
use crate::tui::model::{
    update, Account, AskEvent, Cmd, FinderEvent, Folder, Key, MessageRow, Model, Msg, Overlay,
    ReplyEvent, SearchEvent, Stream,
};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// The queued undo toast, if [`Model::shown_toast`] is showing one — these
/// tests only ever arm at most one, so unwrapping past a non-`Undo` variant
/// would itself be the failure worth seeing.
fn undo_toast(model: &Model) -> Option<&UndoToast> {
    match model.shown_toast() {
        Some(Toast::Undo(toast)) => Some(toast),
        _ => None,
    }
}

fn account() -> Account {
    Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    }
}

fn folders() -> Vec<Folder> {
    vec![
        Folder {
            id: 1,
            name: "INBOX".to_owned(),
            message_count: 3,
        },
        Folder {
            id: 2,
            name: "Archive".to_owned(),
            message_count: 9,
        },
    ]
}

fn row(id: i64) -> MessageRow {
    MessageRow {
        id,
        subject: format!("subject {id}"),
        from: "Alice".to_owned(),
        from_addr: Some("alice@example.com".to_owned()),
        date: Some(1_700_000_000 + id),
        flags: Vec::new(),
        has_attachments: false,
    }
}

/// A model with an account, folders and three messages — what every overlay
/// opens on top of.
fn loaded() -> Model {
    let mut model = Model::new();
    model.account = Some(account());
    model.folders = folders();
    model.open_folder = Some(1);
    model.messages = vec![row(10), row(11), row(12)];
    model
}

fn press(model: &mut Model, key: Key) -> Vec<Cmd> {
    update(model, Msg::Key(key))
}

fn keys(model: &mut Model, sequence: &str) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    for c in sequence.chars() {
        cmds.extend(press(model, Key::Char(c)));
    }
    cmds
}

/// The generation of whichever streaming command `cmds` carries.
///
/// Tests read it from the command rather than from the model because the
/// counter is private — which is the point: a client's only handle on "which
/// query is this" is the one the command was stamped with.
fn stream_generation(cmds: &[Cmd]) -> u64 {
    // The *last* one: `keys` types a whole word and every character issues a
    // query, so the generation that matters is the one the final keystroke
    // was stamped with — every earlier one has already been superseded.
    for cmd in cmds.iter().rev() {
        match cmd {
            Cmd::Search { generation, .. }
            | Cmd::Find { generation, .. }
            | Cmd::Ask { generation, .. } => return *generation,
            _ => {}
        }
    }
    panic!("no streaming command in {cmds:?}");
}

fn hit(message_id: i64, subject: &str) -> Hit {
    Hit {
        message_id,
        subject: subject.to_owned(),
        from: "Alice".to_owned(),
        date: Some(1_700_000_000),
        snippet: format!("about {subject}"),
        highlights: Vec::new(),
        sources: vec!["lexical".to_owned()],
    }
}

fn item(kind: FinderKind, ref_id: i64, primary: &str) -> FinderItem {
    FinderItem {
        kind,
        ref_id,
        primary: primary.to_owned(),
        secondary: String::new(),
        positions: Vec::new(),
        mailbox_id: 0,
    }
}

fn batch(model: &mut Model, generation: u64, items: Vec<FinderItem>, complete: bool) {
    update(
        model,
        Msg::Finder {
            generation,
            event: FinderEvent::Batch {
                items,
                complete,
                superseded: false,
                scanned: 10,
            },
        },
    );
}

fn outbox_row(id: i64, state: &str, undo_deadline: Option<i64>) -> OutboxRow {
    OutboxRow {
        id,
        to: format!("bob{id}@example.com"),
        subject: format!("draft {id}"),
        state: state.to_owned(),
        send_at: 1_700_000_100,
        undo_deadline,
        last_error: None,
    }
}

fn search_pane(model: &Model) -> &SearchPane {
    match model.overlay.as_ref() {
        Some(Overlay::Search(pane)) => pane,
        other => panic!("expected the search overlay, found {other:?}"),
    }
}

fn finder_pane(model: &Model) -> &FinderPane {
    match model.overlay.as_ref() {
        Some(Overlay::Finder(pane)) => pane,
        other => panic!("expected the finder overlay, found {other:?}"),
    }
}

fn command_pane(model: &Model) -> &CommandPane {
    match model.overlay.as_ref() {
        Some(Overlay::Command(pane)) => pane,
        other => panic!("expected the command overlay, found {other:?}"),
    }
}

fn ask_pane(model: &Model) -> &AskPane {
    match model.overlay.as_ref() {
        Some(Overlay::Ask(pane)) => pane,
        other => panic!("expected the ask overlay, found {other:?}"),
    }
}

/// Open the search overlay on `query`, and answer with `hits`.
fn searched(query: &str, hits: Vec<Hit>) -> Model {
    let mut model = loaded();
    press(&mut model, Key::Char('/'));
    let cmds = keys(&mut model, query);
    let generation = stream_generation(&cmds);
    for one in hits {
        update(
            &mut model,
            Msg::Search {
                generation,
                event: SearchEvent::Hit(Box::new(one)),
            },
        );
    }
    update(
        &mut model,
        Msg::Search {
            generation,
            event: SearchEvent::Done(Ok(())),
        },
    );
    model
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

#[test]
fn slash_opens_the_search_overlay_and_every_keystroke_issues_a_fresh_query() {
    let mut model = loaded();
    assert!(press(&mut model, Key::Char('/')).is_empty());
    assert_eq!(search_pane(&model).query, "");

    let first = press(&mut model, Key::Char('a'));
    let second = press(&mut model, Key::Char('b'));
    assert_eq!(
        first,
        vec![Cmd::Search {
            query: "a".to_owned(),
            generation: stream_generation(&first),
            account_id: 7,
        }]
    );
    assert_eq!(search_pane(&model).query, "ab");
    assert!(
        stream_generation(&second) > stream_generation(&first),
        "each keystroke supersedes the last; the executor debounces and aborts"
    );
}

#[test]
fn a_hit_from_a_superseded_query_is_dropped() {
    let mut model = loaded();
    press(&mut model, Key::Char('/'));
    let stale = stream_generation(&press(&mut model, Key::Char('a')));
    let live = stream_generation(&press(&mut model, Key::Char('b')));

    update(
        &mut model,
        Msg::Search {
            generation: stale,
            event: SearchEvent::Hit(Box::new(hit(1, "from the query nobody is running"))),
        },
    );
    assert!(
        search_pane(&model).hits.is_empty(),
        "a frame stamped with an abandoned generation is data about a query nobody asked for"
    );

    update(
        &mut model,
        Msg::Search {
            generation: live,
            event: SearchEvent::Hit(Box::new(hit(2, "the live one"))),
        },
    );
    assert_eq!(search_pane(&model).hits.len(), 1);
}

#[test]
fn search_sigils_and_operators_reach_the_daemon_untouched() {
    // The `~`/`=` grammar has exactly one implementation, server-side. The
    // overlay must not inspect, strip or reorder any of it.
    for query in ["~contract termination", "=Q3 report", "-tag:news from:a@b"] {
        let mut model = loaded();
        press(&mut model, Key::Char('/'));
        let cmds = keys(&mut model, query);
        let last = cmds
            .last()
            .unwrap_or_else(|| panic!("no command for {query}"));
        match last {
            Cmd::Search { query: sent, .. } => assert_eq!(sent, query),
            other => panic!("expected a search, found {other:?}"),
        }
    }
}

#[test]
fn tab_completes_an_unambiguous_operator_and_reissues_the_query() {
    let mut model = loaded();
    press(&mut model, Key::Char('/'));
    keys(&mut model, "acme fr");
    let cmds = press(&mut model, Key::Tab);
    assert_eq!(search_pane(&model).query, "acme from:");
    match cmds.first() {
        Some(Cmd::Search { query, .. }) => assert_eq!(query, "acme from:"),
        other => panic!("completing should re-run the search, got {other:?}"),
    }
}

#[test]
fn tab_never_guesses_between_two_operators() {
    let mut model = loaded();
    press(&mut model, Key::Char('/'));
    keys(&mut model, "t");
    // `t` is `to:` and `tag:` and `thread:`. Completing to one of them would
    // be a keystroke that did the wrong thing rather than one that did
    // nothing.
    assert!(press(&mut model, Key::Tab).is_empty());
    assert_eq!(search_pane(&model).query, "t");
}

#[test]
fn enter_hands_the_keyboard_to_the_results_and_x_opens_the_why_panel() {
    let mut model = searched("acme", vec![hit(10, "one"), hit(11, "two")]);
    assert!(search_pane(&model).typing());

    press(&mut model, Key::Enter);
    assert_eq!(search_pane(&model).focus, SearchFocus::Results);

    // `x` is a command here and a character on the query line — which is the
    // whole reason the results have a mode of their own.
    let cmds = press(&mut model, Key::Char('x'));
    assert!(search_pane(&model).explain);
    assert_eq!(
        cmds,
        vec![Cmd::Explain {
            query: "acme".to_owned(),
            message_id: 10,
            account_id: 7,
        }]
    );
}

#[test]
fn the_why_panel_follows_the_cursor() {
    let mut model = searched("acme", vec![hit(10, "one"), hit(11, "two")]);
    press(&mut model, Key::Enter);
    press(&mut model, Key::Char('x'));
    update(
        &mut model,
        Msg::Explained {
            message_id: 10,
            result: Ok(Explanation {
                message_id: 10,
                score: "1.000".to_owned(),
                features: Vec::new(),
                sources: Vec::new(),
                matched: None,
                claude_reason: String::new(),
            }),
        },
    );

    let cmds = press(&mut model, Key::Char('j'));
    assert_eq!(
        cmds,
        vec![Cmd::Explain {
            query: "acme".to_owned(),
            message_id: 11,
            account_id: 7,
        }],
        "moving the cursor re-explains, without the user asking again"
    );
    assert!(
        search_pane(&model).explanation.is_none(),
        "the previous hit's breakdown must not sit under the new hit"
    );
}

#[test]
fn a_stale_explanation_never_lands_under_a_different_hit() {
    let mut model = searched("acme", vec![hit(10, "one"), hit(11, "two")]);
    press(&mut model, Key::Enter);
    press(&mut model, Key::Char('x'));
    press(&mut model, Key::Char('j'));

    update(
        &mut model,
        Msg::Explained {
            message_id: 10,
            result: Ok(Explanation {
                message_id: 10,
                score: "9.999".to_owned(),
                features: Vec::new(),
                sources: Vec::new(),
                matched: None,
                claude_reason: String::new(),
            }),
        },
    );
    assert!(search_pane(&model).explanation.is_none());
}

#[test]
fn enter_on_a_result_opens_that_message() {
    let mut model = searched("acme", vec![hit(10, "one"), hit(44, "two")]);
    press(&mut model, Key::Enter);
    press(&mut model, Key::Char('j'));
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(cmds, vec![Cmd::Open { message_id: 44 }]);
    assert!(
        model.overlay.is_none(),
        "opening a result leaves the overlay behind"
    );
}

// ---------------------------------------------------------------------------
// finder
// ---------------------------------------------------------------------------

#[test]
fn ctrl_p_opens_the_finder_and_asks_for_the_recents_immediately() {
    let mut model = loaded();
    let cmds = press(&mut model, Key::ctrl('p'));
    // An empty finder query means "rank by signals alone", which is the
    // recents-and-frequents list a picker should already be showing.
    match cmds.first() {
        Some(Cmd::Find { query, .. }) => assert_eq!(query, ""),
        other => panic!("expected an immediate find, got {other:?}"),
    }
}

#[test]
fn a_finder_batch_replaces_the_list_rather_than_appending_to_it() {
    let mut model = loaded();
    let generation = stream_generation(&press(&mut model, Key::ctrl('p')));

    batch(
        &mut model,
        generation,
        vec![
            item(FinderKind::Message, 1, "first"),
            item(FinderKind::Message, 2, "second"),
        ],
        false,
    );
    // A bounded top-K heap can evict what it already sent, so the second
    // snapshot is authoritative on its own. Appending would keep showing a
    // result the daemon has since rejected.
    batch(
        &mut model,
        generation,
        vec![item(FinderKind::Message, 3, "third")],
        true,
    );

    let pane = finder_pane(&model);
    assert_eq!(pane.items.len(), 1);
    assert_eq!(pane.items[0].primary, "third");
    assert!(pane.complete);
}

#[test]
fn a_finder_batch_from_a_superseded_query_is_dropped() {
    let mut model = loaded();
    let stale = stream_generation(&press(&mut model, Key::ctrl('p')));
    let live = stream_generation(&press(&mut model, Key::Char('a')));

    batch(
        &mut model,
        stale,
        vec![item(FinderKind::Message, 1, "stale")],
        true,
    );
    assert!(finder_pane(&model).items.is_empty());

    batch(
        &mut model,
        live,
        vec![item(FinderKind::Message, 2, "live")],
        true,
    );
    assert_eq!(finder_pane(&model).items.len(), 1);
}

#[test]
fn a_superseded_finder_stream_is_never_reported_as_an_error() {
    let mut model = loaded();
    let generation = stream_generation(&press(&mut model, Key::ctrl('p')));
    update(
        &mut model,
        Msg::Finder {
            generation,
            event: FinderEvent::Batch {
                items: Vec::new(),
                complete: true,
                superseded: true,
                scanned: 3,
            },
        },
    );
    let pane = finder_pane(&model);
    assert!(pane.superseded);
    assert!(
        pane.error.is_none(),
        "supersession ends a stream cleanly; it is not a failure to show the user"
    );
}

#[test]
fn the_finder_jumps_by_kind() {
    // A message opens; a folder becomes the open folder; a tag and a contact
    // become the search operator that selects them.
    let mut model = loaded();
    let generation = stream_generation(&press(&mut model, Key::ctrl('p')));
    batch(
        &mut model,
        generation,
        vec![item(FinderKind::Message, 42, "a message")],
        true,
    );
    assert_eq!(
        press(&mut model, Key::Enter),
        vec![Cmd::Open { message_id: 42 }]
    );

    let mut model = loaded();
    let generation = stream_generation(&press(&mut model, Key::ctrl('p')));
    batch(
        &mut model,
        generation,
        vec![item(FinderKind::Mailbox, 2, "Archive")],
        true,
    );
    assert_eq!(
        press(&mut model, Key::Enter),
        vec![Cmd::LoadMessages { mailbox_id: 2 }]
    );
    assert!(model.overlay.is_none());

    let mut model = loaded();
    let generation = stream_generation(&press(&mut model, Key::ctrl('p')));
    batch(
        &mut model,
        generation,
        vec![item(FinderKind::Tag, 3, "work")],
        true,
    );
    let cmds = press(&mut model, Key::Enter);
    match cmds.first() {
        Some(Cmd::Search { query, .. }) => assert_eq!(query, "tag:work"),
        other => panic!("a tag should become a search, got {other:?}"),
    }
}

#[test]
fn a_finder_command_row_runs_the_action_it_names() {
    let mut model = loaded();
    let generation = stream_generation(&press(&mut model, Key::ctrl('p')));
    let mut command = item(FinderKind::Command, 1, "show help");
    command.secondary = "help".to_owned();
    batch(&mut model, generation, vec![command], true);

    press(&mut model, Key::Enter);
    assert!(matches!(model.overlay, Some(Overlay::Help(_))));
}

#[test]
fn a_finder_command_this_build_does_not_have_is_refused_by_name() {
    let mut model = loaded();
    let generation = stream_generation(&press(&mut model, Key::ctrl('p')));
    let mut command = item(FinderKind::Command, 1, "from a newer daemon");
    command.secondary = "message.teleport".to_owned();
    batch(&mut model, generation, vec![command], true);

    assert!(press(&mut model, Key::Enter).is_empty());
    assert!(
        model.status.contains("message.teleport"),
        "an unknown action id is named, not silently ignored: {}",
        model.status
    );
}

#[test]
fn a_finder_kind_this_build_does_not_know_is_never_acted_on() {
    // `ref_id` is a row id in whichever table the kind names, and those id
    // spaces overlap — acting on an unknown kind would act on the wrong
    // object rather than on nothing.
    let mut model = loaded();
    let generation = stream_generation(&press(&mut model, Key::ctrl('p')));
    batch(
        &mut model,
        generation,
        vec![item(FinderKind::Unknown, 10, "something new")],
        true,
    );
    assert!(press(&mut model, Key::Enter).is_empty());
    assert!(model.overlay.is_some(), "and the overlay stays put");
}

// ---------------------------------------------------------------------------
// the command line's ranked matches
// ---------------------------------------------------------------------------

#[test]
fn the_command_line_resolves_typed_text_to_verbs() {
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    assert_eq!(
        command_pane(&model).matches.len(),
        command::children_of(&[]).len(),
        "an empty line lists every verb — it is a discovery surface, not only \
         a shortcut"
    );

    keys(&mut model, "arch");
    let matches = &command_pane(&model).matches;
    assert_eq!(
        matches.first().map(|entry| entry.verb.as_str()),
        Some("message archive")
    );
    assert!(
        matches.iter().all(|entry| entry.verb != "cursor top"),
        "a command that matches nothing typed is not offered"
    );
}

#[test]
fn the_ranked_list_ignores_the_range_the_bang_and_the_flags() {
    // Ranking runs on every keystroke, including on lines the parser would
    // reject outright, so what it reads is the verb-shaped part of the line
    // and nothing else.
    let keymap = Keymap::defaults();
    let plain = command_matches("message archive", &keymap);
    for line in [
        "'<,'>message archive",
        "%message archive",
        "20message archive",
        "message archive!",
        "message archive --dry-run",
        "MESSAGE.ARCHIVE",
    ] {
        assert_eq!(
            command_matches(line, &keymap)
                .first()
                .map(|e| e.verb.clone()),
            plain.first().map(|e| e.verb.clone()),
            "{line:?} ranked differently"
        );
    }
}

#[test]
fn the_command_line_runs_the_best_match_when_the_line_names_no_verb() {
    // Task 85's palette, through the same pane: `help` is not a verb path
    // anybody typed in full, and Enter still runs it.
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    keys(&mut model, "hel");
    press(&mut model, Key::Enter);
    assert!(
        matches!(model.overlay, Some(Overlay::Help(_))),
        "the command line closes and the command runs against the screen \
         behind it"
    );
}

#[test]
fn the_command_line_says_so_when_nothing_matches() {
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    keys(&mut model, "zzzznope");
    assert!(command_pane(&model).matches.is_empty());
    assert!(press(&mut model, Key::Enter).is_empty());
    let pane = command_pane(&model);
    assert!(
        pane.error
            .as_deref()
            .is_some_and(|why| why.contains("no command")),
        "{:?}",
        pane.error
    );
    assert!(model.overlay.is_some(), "and the overlay stays open");
}

#[test]
fn every_command_entry_names_a_verb_this_build_can_run() {
    // The list's whole vocabulary is the verb registry, so this is the
    // property that keeps a row from being a dead end.
    for entry in command_matches("", &Keymap::defaults()) {
        let path: Vec<&str> = entry.verb.split(' ').collect();
        assert!(
            command::verb_at(&path).is_some(),
            "{:?} is not in the registry",
            entry.verb
        );
    }
}

#[test]
fn a_verb_no_chord_reaches_is_still_offered_without_a_key_column() {
    // `helpgrep` has no binding at all — it exists only in the grammar. The
    // palette could not offer it, because its vocabulary was `Action::ALL`;
    // this list's is the registry, which is strictly wider.
    let entries = command_matches("helpgrep", &Keymap::defaults());
    let found = entries
        .iter()
        .find(|entry| entry.verb == "helpgrep")
        .expect("helpgrep is a verb");
    assert!(found.chords.is_empty(), "{:?}", found.chords);
}

// ---------------------------------------------------------------------------
// ask pane
// ---------------------------------------------------------------------------

/// Ask a question and stream a whole answer back.
fn asked(grounded: bool, citations: Vec<Citation>) -> Model {
    let mut model = loaded();
    press(&mut model, Key::Char('A'));
    keys(&mut model, "who owes me money");
    let cmds = press(&mut model, Key::Enter);
    let generation = stream_generation(&cmds);

    for event in [
        AskEvent::Trace("retrieved 9 · packed 4".to_owned()),
        AskEvent::Token("Acme ".to_owned()),
        AskEvent::Token("owes you [1].".to_owned()),
    ] {
        update(&mut model, Msg::Ask { generation, event });
    }
    for citation in citations {
        update(
            &mut model,
            Msg::Ask {
                generation,
                event: AskEvent::Cite(Box::new(citation)),
            },
        );
    }
    update(
        &mut model,
        Msg::Ask {
            generation,
            event: AskEvent::Done {
                grounded,
                refusal: if grounded {
                    String::new()
                } else {
                    "nothing in your mail says".to_owned()
                },
            },
        },
    );
    model
}

fn citation(label: u32, message_id: i64) -> Citation {
    Citation {
        label,
        message_id,
        subject: format!("invoice {label}"),
        from_addr: "billing@acme.com".to_owned(),
        mailbox: "INBOX".to_owned(),
        quote: "Total $4,200".to_owned(),
    }
}

#[test]
fn the_ask_pane_streams_trace_then_prose_then_citations() {
    let model = asked(true, vec![citation(1, 501)]);
    let pane = ask_pane(&model);
    assert_eq!(pane.phase, AskPhase::Done);
    assert_eq!(pane.trace.as_deref(), Some("retrieved 9 · packed 4"));
    assert_eq!(pane.answer, "Acme owes you [1].");
    // Citations arrive after the prose because an inline `[n]` is only
    // resolvable once the whole answer has been seen.
    assert_eq!(pane.citations.len(), 1);
    assert_eq!(pane.citations[0].message_id, 501);
    assert!(pane.grounded);
}

#[test]
fn an_ungrounded_answer_is_reported_as_the_daemons_verdict() {
    let model = asked(false, Vec::new());
    let pane = ask_pane(&model);
    assert!(!pane.grounded);
    assert_eq!(pane.refusal, "nothing in your mail says");
    assert!(
        model.status.contains("daemon"),
        "`grounded` is the daemon's verdict, not the model's claim about itself: {}",
        model.status
    );
}

#[test]
fn enter_on_a_citation_opens_the_message_it_names() {
    let mut model = asked(true, vec![citation(1, 501), citation(2, 502)]);
    press(&mut model, Key::Char('j'));
    assert_eq!(
        press(&mut model, Key::Enter),
        vec![Cmd::Open { message_id: 502 }]
    );
}

#[test]
fn an_ask_that_fails_leaves_the_pane_usable() {
    let mut model = loaded();
    press(&mut model, Key::Char('A'));
    keys(&mut model, "anything");
    let generation = stream_generation(&press(&mut model, Key::Enter));
    update(
        &mut model,
        Msg::Ask {
            generation,
            event: AskEvent::Failed("the provider is down".to_owned()),
        },
    );
    assert_eq!(ask_pane(&model).phase, AskPhase::Done);
    assert!(model.status.contains("the provider is down"));
    // And Esc still works, which is the property that matters.
    press(&mut model, Key::Esc);
    assert!(model.overlay.is_none());
}

#[test]
fn an_empty_question_is_refused_rather_than_sent() {
    let mut model = loaded();
    press(&mut model, Key::Char('A'));
    assert!(press(&mut model, Key::Enter).is_empty());
    assert_eq!(ask_pane(&model).phase, AskPhase::Asking);
}

#[test]
fn a_token_from_a_superseded_ask_is_dropped() {
    let mut model = asked(true, vec![citation(1, 501)]);
    update(
        &mut model,
        Msg::Ask {
            generation: 999,
            event: AskEvent::Token(" and so does everyone else".to_owned()),
        },
    );
    assert_eq!(ask_pane(&model).answer, "Acme owes you [1].");
}

// ---------------------------------------------------------------------------
// reply pane (task 100)
// ---------------------------------------------------------------------------

/// Type a `:` line and run it, the same way `commands::tests::run` does —
/// duplicated rather than shared, since that helper is private to a sibling
/// module and this file already has its own `press`/`keys`.
fn typed(model: &mut Model, line: &str) -> Vec<Cmd> {
    press(model, Key::Char(':'));
    keys(model, line);
    press(model, Key::Enter)
}

/// A typed `:` line's own commands, ignoring the history write that rides
/// along with every line that parses — refused or not, `commands::tests`'
/// own `issued` is the precedent for stripping it before comparing.
fn issued(cmds: &[Cmd]) -> Vec<Cmd> {
    cmds.iter()
        .filter(|cmd| !matches!(cmd, Cmd::SaveHistory { .. }))
        .cloned()
        .collect()
}

fn reply_pane(model: &Model) -> &ReplyPane {
    match model.overlay.as_ref() {
        Some(Overlay::Reply(pane)) => pane,
        other => panic!("expected the reply overlay, found {other:?}"),
    }
}

/// `:reply --ai` on message 10, streamed to completion.
fn replied(intent: &str) -> Model {
    let mut model = loaded();
    let cmds = typed(&mut model, &format!("reply --ai {intent}"));
    let generation = stream_generation_reply(&cmds);
    for event in [
        ReplyEvent::Context("2 thread message(s)".to_owned()),
        ReplyEvent::Token("Sounds good, ".to_owned()),
        ReplyEvent::Token("see you then.".to_owned()),
    ] {
        update(&mut model, Msg::Reply { generation, event });
    }
    update(
        &mut model,
        Msg::Reply {
            generation,
            event: ReplyEvent::Drafted {
                draft_id: 42,
                to: "alice@example.com".to_owned(),
            },
        },
    );
    update(
        &mut model,
        Msg::Reply {
            generation,
            event: ReplyEvent::Done,
        },
    );
    model
}

fn stream_generation_reply(cmds: &[Cmd]) -> u64 {
    for cmd in cmds {
        if let Cmd::DraftReply { generation, .. } = cmd {
            return *generation;
        }
    }
    panic!("no Cmd::DraftReply in {cmds:?}");
}

#[test]
fn the_reply_pane_streams_context_then_prose_then_the_drafted_id() {
    let model = replied("push to tuesday");
    let pane = reply_pane(&model);
    assert!(pane.done);
    assert_eq!(pane.context.as_deref(), Some("2 thread message(s)"));
    assert_eq!(pane.body, "Sounds good, see you then.");
    assert_eq!(pane.drafted, Some((42, "alice@example.com".to_owned())));
    assert!(
        model.status.contains("42"),
        "the status line names the draft it created: {}",
        model.status
    );
}

#[test]
fn a_reply_that_fails_leaves_the_pane_usable() {
    let mut model = loaded();
    let cmds = typed(&mut model, "reply --ai anything");
    let generation = stream_generation_reply(&cmds);
    update(
        &mut model,
        Msg::Reply {
            generation,
            event: ReplyEvent::Failed("the provider is down".to_owned()),
        },
    );
    assert!(reply_pane(&model).done);
    assert!(model.status.contains("the provider is down"));
    press(&mut model, Key::Esc);
    assert!(model.overlay.is_none());
}

#[test]
fn a_token_from_a_superseded_reply_is_dropped() {
    let mut model = replied("push to tuesday");
    update(
        &mut model,
        Msg::Reply {
            generation: 999,
            event: ReplyEvent::Token(" and one more thing".to_owned()),
        },
    );
    assert_eq!(reply_pane(&model).body, "Sounds good, see you then.");
}

#[test]
fn leaving_the_reply_pane_cancels_the_model_call() {
    // The same reason leaving the ask pane does: `DraftReply` is a retrieval
    // pass plus a model completion, and letting it run after Esc bills for a
    // reply nothing will draw.
    let mut model = loaded();
    typed(&mut model, "reply --ai anything");
    assert_eq!(
        press(&mut model, Key::Esc),
        vec![Cmd::CancelStream {
            which: Stream::Reply
        }]
    );
}

#[test]
fn bare_reply_drafts_the_same_way_r_does() {
    let mut typed_model = loaded();
    let typed_cmds = issued(&typed(&mut typed_model, "reply"));

    let mut keyed_model = loaded();
    let keyed_cmds = press(&mut keyed_model, Key::Char('r'));

    assert_eq!(
        typed_cmds, keyed_cmds,
        "`:reply` and `r` are the same request to the daemon"
    );
    assert!(
        !matches!(typed_model.overlay, Some(Overlay::Reply(_))),
        "no --ai, no streaming pane"
    );
}

#[test]
fn reply_all_without_ai_is_refused() {
    let mut model = loaded();
    let cmds = issued(&typed(&mut model, "reply --reply-all"));
    assert!(cmds.is_empty(), "{cmds:?}");
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("--ai"), "{why}");
}

#[test]
fn an_intent_without_ai_is_refused() {
    let mut model = loaded();
    let cmds = issued(&typed(&mut model, "reply see you then"));
    assert!(cmds.is_empty(), "{cmds:?}");
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("--ai"), "{why}");
}

#[test]
fn reply_ai_with_no_message_selected_is_refused() {
    // Through `single_target`, the same as bare `:reply` — see
    // `reply_ai_and_bare_reply_agree_on_a_visual_selection` below — so the
    // command line is closed and the refusal lands on the status line,
    // exactly where `r` would put it, rather than as a command-line error.
    let mut model = loaded();
    model.messages.clear();
    let cmds = issued(&typed(&mut model, "reply --ai anything"));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(model.overlay.is_none());
    assert!(model.status.contains("message"), "{}", model.status);
}

#[test]
fn reply_ai_and_bare_reply_agree_on_a_visual_selection() {
    // Before the fix this pins, `--ai` read `target_message` directly and
    // drafted from the cursor row while a selection was up; bare `:reply`
    // (via `reply` -> `single_target`) already refused it. One verb, one
    // rule for what "the target" means, regardless of the flag.
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    let cmds = issued(&typed(&mut model, "reply --ai anything"));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(model.status.contains("one message"), "{}", model.status);
}

// ---------------------------------------------------------------------------
// outbox and the undo toast
// ---------------------------------------------------------------------------

#[test]
fn the_outbox_lists_and_raises_a_toast_for_a_send_still_inside_its_window() {
    let mut model = loaded();
    let cmds = press(&mut model, Key::Char('O'));
    assert_eq!(cmds, vec![Cmd::LoadOutbox { account_id: 7 }]);

    let cmds = update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![
                outbox_row(1, "scheduled", Some(1_010)),
                outbox_row(2, "scheduled", Some(1_005)),
            ]),
        },
    );
    let toast = undo_toast(&model).unwrap_or_else(|| panic!("no toast"));
    assert_eq!(
        toast.outbox_id, 2,
        "the window about to close is the one a countdown is for"
    );
    assert_eq!(toast.remaining, 5);
    assert_eq!(cmds, vec![Cmd::Countdown { until: 1_005 }]);
}

#[test]
fn a_sent_or_cancelled_entry_never_raises_a_toast() {
    // Both can still carry an `undo_deadline` in the future; offering to undo
    // one of them would be an offer that cannot be honoured.
    for state in ["sent", "canceled", "failed", "sending"] {
        let mut model = loaded();
        update(
            &mut model,
            Msg::Outbox {
                now: 1_000,
                result: Ok(vec![outbox_row(1, state, Some(1_010))]),
            },
        );
        assert!(model.toasts.is_empty(), "{state} must not be undoable");
    }
}

#[test]
fn the_toast_counts_down_and_retires_when_the_window_closes() {
    let mut model = loaded();
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![outbox_row(1, "scheduled", Some(1_003))]),
        },
    );
    update(&mut model, Msg::Tick(1_001));
    assert_eq!(undo_toast(&model).map(|toast| toast.remaining), Some(2));

    update(&mut model, Msg::Tick(1_003));
    assert!(
        model.toasts.is_empty(),
        "an undo offer that no longer works is worse than none"
    );
}

#[test]
fn u_cancels_the_send_the_toast_names_from_the_message_list() {
    let mut model = loaded();
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![outbox_row(9, "scheduled", Some(1_010))]),
        },
    );
    let cmds = press(&mut model, Key::Char('u'));
    assert_eq!(cmds, vec![Cmd::CancelSend { outbox_id: 9 }]);
    assert!(
        model.toasts.is_empty(),
        "the offer is taken, so it stops being offered"
    );
}

#[test]
fn u_in_the_outbox_cancels_the_highlighted_row() {
    let mut model = loaded();
    press(&mut model, Key::Char('O'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![
                outbox_row(1, "scheduled", None),
                outbox_row(2, "scheduled", None),
            ]),
        },
    );
    press(&mut model, Key::Char('j'));
    assert_eq!(
        press(&mut model, Key::Char('u')),
        vec![Cmd::CancelSend { outbox_id: 2 }]
    );
}

#[test]
fn u_refuses_a_send_that_has_already_gone() {
    let mut model = loaded();
    press(&mut model, Key::Char('O'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![outbox_row(1, "sent", None)]),
        },
    );
    assert!(press(&mut model, Key::Char('u')).is_empty());
    assert!(model.status.contains("already sent"), "{}", model.status);
}

#[test]
fn u_with_nothing_to_undo_says_so() {
    let mut model = loaded();
    assert!(press(&mut model, Key::Char('u')).is_empty());
    assert!(model.status.contains("nothing to undo"), "{}", model.status);
}

// ---------------------------------------------------------------------------
// AI panel and the quick-action menu
// ---------------------------------------------------------------------------

#[test]
fn the_ai_panel_loads_the_message_under_the_cursor_and_follows_it() {
    let mut model = loaded();
    let cmds = press(&mut model, Key::Char('\\'));
    assert!(model.ai_panel);
    assert_eq!(
        cmds,
        vec![Cmd::LoadSummary {
            message_id: 10,
            suggest_reply: false,
        }]
    );

    update(
        &mut model,
        Msg::Summarized {
            message_id: 10,
            result: Ok(AiSummary {
                message_id: 10,
                status: "ok".to_owned(),
                tl_dr: Some("an invoice".to_owned()),
                ..AiSummary::default()
            }),
        },
    );
    assert_eq!(model.summary.as_ref().map(|s| s.message_id), Some(10));

    let cmds = press(&mut model, Key::Char('j'));
    assert_eq!(
        cmds,
        vec![Cmd::LoadSummary {
            message_id: 11,
            suggest_reply: false,
        }]
    );
    assert!(
        model.summary.is_none(),
        "the previous message's analysis must not sit under the new message"
    );
}

#[test]
fn the_ai_panel_does_not_re_request_while_a_load_is_in_flight() {
    let mut model = loaded();
    press(&mut model, Key::Char('\\'));
    // Any message at all, without a response landing first.
    assert!(update(&mut model, Msg::Changed)
        .iter()
        .all(|cmd| !matches!(cmd, Cmd::LoadSummary { .. })));
}

#[test]
fn hiding_the_ai_panel_drops_what_it_was_showing() {
    let mut model = loaded();
    press(&mut model, Key::Char('\\'));
    update(
        &mut model,
        Msg::Summarized {
            message_id: 10,
            result: Ok(AiSummary {
                message_id: 10,
                ..AiSummary::default()
            }),
        },
    );
    press(&mut model, Key::Char('\\'));
    assert!(!model.ai_panel);
    assert!(model.summary.is_none());
}

#[test]
fn the_quick_menu_captures_its_message_when_it_opens() {
    let mut model = loaded();
    press(&mut model, Key::Char('.'));
    match model.overlay.as_ref() {
        Some(Overlay::Quick(pane)) => {
            assert_eq!(pane.message_id, 10);
            assert_eq!(pane.subject, "subject 10");
        }
        other => panic!("expected the quick menu, found {other:?}"),
    }

    // The list is live, and a reload can move the cursor while the menu is
    // up. What the menu acts on is what it captured, not what the cursor now
    // points at.
    model.messages = vec![row(99), row(10)];
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::LoadSummary {
            message_id: 10,
            suggest_reply: false,
        }]
    );
}

#[test]
fn the_quick_menu_opens_the_ask_pane_with_the_subject_already_typed() {
    let mut model = loaded();
    press(&mut model, Key::Char('.'));
    press(&mut model, Key::Char('j'));
    press(&mut model, Key::Enter);
    assert_eq!(ask_pane(&model).question, "About \"subject 10\": ");
}

#[test]
fn the_quick_menu_is_where_the_calls_that_cost_money_live() {
    let mut model = loaded();
    press(&mut model, Key::Char('.'));
    press(&mut model, Key::Char('G'));
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::LoadSummary {
            message_id: 10,
            suggest_reply: true,
        }],
        "a reply suggestion is behind a menu, never on a bare key"
    );
}

// ---------------------------------------------------------------------------
// no overlay may wedge the UI
// ---------------------------------------------------------------------------

/// Open each overlay in turn and hand the caller the model it is up in.
fn each_overlay(mut check: impl FnMut(&str, Model)) {
    for (name, key) in [
        ("search", Key::Char('/')),
        ("finder", Key::ctrl('p')),
        ("palette", Key::ctrl('k')),
        ("ask", Key::Char('A')),
        ("outbox", Key::Char('O')),
        ("quick", Key::Char('.')),
        ("help", Key::Char('?')),
    ] {
        let mut model = loaded();
        press(&mut model, key);
        assert!(model.overlay.is_some(), "{name} did not open");
        check(name, model);
    }
}

#[test]
fn esc_leaves_every_overlay() {
    each_overlay(|name, mut model| {
        press(&mut model, Key::Esc);
        assert!(model.overlay.is_none(), "Esc did not leave {name}");
        assert!(!model.quit, "Esc must not quit from {name}");
    });
}

#[test]
fn ctrl_c_quits_from_every_overlay() {
    each_overlay(|name, mut model| {
        press(&mut model, Key::CTRL_C);
        assert!(model.quit, "Ctrl-C did not quit from {name}");
    });
}

#[test]
fn esc_leaves_an_overlay_even_mid_stream_and_mid_word() {
    // The two states most likely to be special-cased into a trap: a query
    // half-typed, and a stream still arriving.
    let mut model = loaded();
    press(&mut model, Key::Char('/'));
    let generation = stream_generation(&keys(&mut model, "half"));
    update(
        &mut model,
        Msg::Search {
            generation,
            event: SearchEvent::Hit(Box::new(hit(1, "still coming"))),
        },
    );
    press(&mut model, Key::Esc);
    assert!(model.overlay.is_none());
}

#[test]
fn a_half_typed_chord_in_a_menu_does_not_swallow_the_next_key() {
    let mut model = loaded();
    press(&mut model, Key::Char('O'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![
                outbox_row(1, "scheduled", None),
                outbox_row(2, "scheduled", None),
                outbox_row(3, "scheduled", None),
            ]),
        },
    );
    press(&mut model, Key::Char('j'));
    press(&mut model, Key::Char('j'));

    // `g` is the first key of `gg`. The sequence `gk` can never be a binding,
    // so the `g` is dropped and the `k` still moves — eating a keystroke
    // because a chord half-matched is a bug, not a feature.
    press(&mut model, Key::Char('g'));
    press(&mut model, Key::Char('k'));
    match model.overlay.as_ref() {
        Some(Overlay::Outbox(pane)) => assert_eq!(pane.cursor, 1),
        other => panic!("expected the outbox, found {other:?}"),
    }
}

#[test]
fn keys_do_not_reach_the_list_behind_an_overlay() {
    let mut model = loaded();
    press(&mut model, Key::Char('O'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![
                outbox_row(1, "scheduled", None),
                outbox_row(2, "scheduled", None),
            ]),
        },
    );
    press(&mut model, Key::Char('j'));
    assert_eq!(model.message_idx, 0, "the list behind must not have moved");

    // `d` deletes mail in the message list. In a Menu-mode overlay it must be
    // nothing at all — the chain stops at Global by construction.
    assert!(press(&mut model, Key::Char('d')).is_empty());
    assert!(matches!(model.overlay, Some(Overlay::Outbox(_))));
}

#[test]
fn a_typing_overlay_types_the_keys_that_are_commands_elsewhere() {
    let mut model = loaded();
    press(&mut model, Key::ctrl('p'));
    keys(&mut model, "dq3");
    assert_eq!(
        finder_pane(&model).query,
        "dq3",
        "d, q and a digit are text in a prompt — not delete, quit and a count"
    );
    assert_eq!(model.messages.len(), 3, "and nothing was deleted");
}

#[test]
fn a_prompt_is_bounded_against_a_key_held_down() {
    let mut model = loaded();
    press(&mut model, Key::ctrl('p'));
    for _ in 0..(crate::tui::model::MAX_INPUT + 50) {
        press(&mut model, Key::Char('x'));
    }
    assert_eq!(
        finder_pane(&model).query.chars().count(),
        crate::tui::model::MAX_INPUT
    );
}

#[test]
fn an_edit_that_changes_nothing_does_not_re_run_the_query() {
    let mut model = loaded();
    press(&mut model, Key::ctrl('p'));
    // Backspace on an empty prompt. Re-issuing here would be one RPC per
    // repeat of a held-down key, for a string that never moves.
    assert!(press(&mut model, Key::Backspace).is_empty());
}

#[test]
fn an_overlay_key_pressed_over_another_overlay_does_not_discard_it() {
    let mut model = asked(true, vec![citation(1, 501)]);
    // `/` is bound in Menu mode so the search overlay can go back to its
    // query line. Over the ask pane it must do nothing rather than throw an
    // answer away.
    press(&mut model, Key::Char('/'));
    assert!(matches!(model.overlay, Some(Overlay::Ask(_))));
    assert_eq!(ask_pane(&model).answer, "Acme owes you [1].");
}

// ---------------------------------------------------------------------------
// characters, not bytes; and nothing hostile reaches the terminal
// ---------------------------------------------------------------------------

#[test]
fn finder_highlights_index_characters_not_bytes() {
    // `FindResult.positions` are char offsets. Reading them as bytes would
    // highlight the wrong glyph the moment anything is non-ASCII — "café" is
    // five bytes and four characters.
    //
    // The multi-byte character sits *before* the highlighted one on purpose:
    // in "café" alone, char 3 and byte 3 are both the `é`, so a fixture like
    // that passes under either reading and proves nothing. Here char 5 is the
    // `a` of "au" while byte 5 is the space in front of it.
    let runs = runs_from_char_positions("café au", &[5]);
    assert_eq!(
        runs,
        vec![
            ("café ".to_owned(), false),
            ("a".to_owned(), true),
            ("u".to_owned(), false),
        ]
    );

    let runs = runs_from_char_positions("café", &[3]);
    assert_eq!(
        runs,
        vec![("caf".to_owned(), false), ("é".to_owned(), true)]
    );
}

#[test]
fn a_char_position_past_the_end_highlights_nothing() {
    let runs = runs_from_char_positions("café", &[4, 99]);
    assert_eq!(runs, vec![("café".to_owned(), false)]);
}

#[test]
fn a_byte_highlight_that_splits_a_character_is_dropped() {
    // "café" is c-a-f-é where é occupies bytes 3..5. A range ending at 4 lands
    // inside it: dropped, never rounded, because rounding would highlight a
    // different substring than the one that matched.
    assert!(valid_byte_ranges("café", &[(0, 4)]).is_empty());
    assert_eq!(valid_byte_ranges("café", &[(0, 3)]), vec![(0, 3)]);
    assert!(
        valid_byte_ranges("café", &[(0, 99)]).is_empty(),
        "and one past the end is dropped too"
    );
    assert!(
        valid_byte_ranges("café", &[(3, 3)]).is_empty(),
        "as is empty"
    );
}

#[test]
fn byte_highlights_survive_a_control_character_between_them() {
    // The renderer decides highlighting from the character's position in the
    // *original* string and only then sanitizes it. Sanitizing first would
    // shift every highlight after the dropped byte.
    let text = "one \u{1b}two three";
    let two = text.find("two").unwrap_or_default();
    let runs = runs_from_byte_ranges(text, &[(two, two + 3)]);
    let highlighted: String = runs
        .iter()
        .filter(|(_, on)| *on)
        .map(|(text, _)| text.as_str())
        .collect();
    assert_eq!(highlighted, "two");
    let all: String = runs.iter().map(|(text, _)| text.as_str()).collect();
    assert!(!all.contains('\u{1b}'));
}

#[test]
fn truncation_lands_on_a_character_boundary() {
    assert_eq!(truncate_chars("café au lait", 4), "café…");
    assert_eq!(truncate_chars("café", 4), "café");
    assert_eq!(truncate_chars("café", 0), "");
    // Astral-plane characters are one `char` each and must not be split.
    assert_eq!(truncate_chars("👩‍🚀ab", 1), "👩…");
}

#[test]
fn hostile_ansi_and_bidi_never_reach_the_terminal_from_an_overlay() {
    // A subject or a model answer can carry either. An `ESC [` run repaints
    // the screen a TUI owns; a bidi override reorders what the user reads
    // without corrupting anything.
    let hostile = "click \u{1b}[31mhere\u{1b}[0m\u{7} \u{202e}drowssap\u{202c} now";
    for rendered in [safe_line(hostile), safe_prose(hostile)] {
        assert!(
            !rendered.contains('\u{1b}'),
            "raw ESC survived: {rendered:?}"
        );
        assert!(!rendered.contains('\u{7}'), "BEL survived: {rendered:?}");
        assert!(
            !rendered.contains('\u{202e}') && !rendered.contains('\u{202c}'),
            "a bidi override survived: {rendered:?}"
        );
        assert!(
            rendered.contains("click"),
            "and the real text is still there"
        );
        assert!(rendered.contains("now"));
    }
}

#[test]
fn a_row_folds_newlines_and_prose_keeps_them() {
    // The one difference between the two: a subject containing a newline must
    // not shear the row it is drawn in, and a paragraph break in an answer
    // carries meaning.
    assert_eq!(safe_line("one\ntwo"), "one two");
    assert_eq!(safe_prose("one\ntwo"), "one\ntwo");
}

#[test]
fn a_hostile_subject_stays_one_line_through_the_highlighter() {
    let runs = runs_from_char_positions("sub\u{1b}[2Jject", &[0]);
    let all: String = runs.iter().map(|(text, _)| text.as_str()).collect();
    assert!(!all.contains('\u{1b}'));
    assert!(all.starts_with('s'));
}

// ---------------------------------------------------------------------------
// operator completion, in isolation
// ---------------------------------------------------------------------------

#[test]
fn operator_candidates_come_from_the_parsers_own_registry() {
    let names: Vec<&str> = operator_candidates("fr")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(names, vec!["from"]);
    assert!(
        operator_candidates("from:al").is_empty(),
        "a word that already has its colon has chosen its operator"
    );
    assert!(operator_candidates("").is_empty());
    assert!(
        operator_candidates("-fr").is_empty(),
        "a negated term is left alone rather than completed wrongly"
    );
}

#[test]
fn completion_advances_to_the_longest_common_prefix() {
    // `s` is `subject` and `smaller`, whose common prefix is `s` — no
    // progress, so nothing happens.
    assert_eq!(complete_operator("s"), None);
    // `su` is only `subject`.
    assert_eq!(complete_operator("su"), Some("subject:".to_owned()));
    assert_eq!(
        complete_operator("acme su"),
        Some("acme subject:".to_owned()),
        "only the trailing word is touched"
    );
}

// ---------------------------------------------------------------------------
// the loops a follow-the-cursor panel can get into
// ---------------------------------------------------------------------------

#[test]
fn an_explanation_that_failed_is_not_asked_for_again() {
    // `follow_cursor` runs after *every* message. Without somewhere to
    // remember the failure it would see "no explanation, none in flight" and
    // re-issue — at round-trip rate, forever, each one re-running the whole
    // retrieval pipeline server-side.
    let mut model = searched("acme", vec![hit(10, "one")]);
    press(&mut model, Key::Enter);
    press(&mut model, Key::Char('x'));

    let cmds = update(
        &mut model,
        Msg::Explained {
            message_id: 10,
            result: Err("message 10 did not match this query".to_owned()),
        },
    );
    assert!(cmds.is_empty(), "the failure must not re-ask: {cmds:?}");
    assert!(model.status.contains("explain:"), "{}", model.status);

    // Nor on the next message to arrive, whatever it is.
    assert!(update(&mut model, Msg::Changed)
        .iter()
        .all(|cmd| !matches!(cmd, Cmd::Explain { .. })));
}

#[test]
fn a_summary_that_failed_is_not_asked_for_again() {
    let mut model = loaded();
    press(&mut model, Key::Char('\\'));
    let cmds = update(
        &mut model,
        Msg::Summarized {
            message_id: 10,
            result: Err("connection refused".to_owned()),
        },
    );
    assert!(cmds.is_empty(), "the failure must not re-ask: {cmds:?}");
    assert!(update(&mut model, Msg::Changed)
        .iter()
        .all(|cmd| !matches!(cmd, Cmd::LoadSummary { .. })));

    // Moving to a different message still works — the latch is per message,
    // not a dead panel.
    let cmds = press(&mut model, Key::Char('j'));
    assert_eq!(
        cmds,
        vec![Cmd::LoadSummary {
            message_id: 11,
            suggest_reply: false,
        }]
    );
}

#[test]
fn a_pinned_summary_survives_a_reload_that_moves_the_cursor() {
    // `.` aims the panel at a message. A list reloading underneath — which
    // re-clamps the cursor without anyone pressing a key — must not throw the
    // answer away, least of all the paid one.
    let mut model = loaded();
    press(&mut model, Key::Char('.'));
    press(&mut model, Key::Char('G'));
    assert_eq!(
        press(&mut model, Key::Enter),
        vec![Cmd::LoadSummary {
            message_id: 10,
            suggest_reply: true,
        }]
    );

    // The reload moves the cursor off the pinned message entirely.
    model.messages = vec![row(99), row(88)];
    update(&mut model, Msg::Changed);
    let cmds = update(
        &mut model,
        Msg::Summarized {
            message_id: 10,
            result: Ok(AiSummary {
                message_id: 10,
                suggested_reply: Some("Sounds good.".to_owned()),
                ..AiSummary::default()
            }),
        },
    );
    assert!(
        cmds.iter()
            .all(|cmd| !matches!(cmd, Cmd::LoadSummary { .. })),
        "the answer must not be replaced in the same update that delivered it: {cmds:?}"
    );
    assert_eq!(
        model
            .summary
            .as_ref()
            .and_then(|s| s.suggested_reply.clone()),
        Some("Sounds good.".to_owned())
    );

    // A deliberate move releases it and the panel follows again.
    let cmds = press(&mut model, Key::Char('j'));
    assert_eq!(
        cmds,
        vec![Cmd::LoadSummary {
            message_id: 88,
            suggest_reply: false,
        }]
    );
}

// ---------------------------------------------------------------------------
// a stream nobody is reading is stopped, not merely ignored
// ---------------------------------------------------------------------------

#[test]
fn leaving_the_ask_pane_cancels_the_model_call() {
    // A stale *frame* is free to ignore; a stale *stream* is not. AskMailbox
    // is a retrieval pass plus a model completion, and letting it run after
    // Esc bills for an answer nothing will draw.
    let mut model = loaded();
    press(&mut model, Key::Char('A'));
    keys(&mut model, "who owes me");
    press(&mut model, Key::Enter);

    assert_eq!(
        press(&mut model, Key::Esc),
        vec![Cmd::CancelStream { which: Stream::Ask }]
    );
}

#[test]
fn leaving_the_search_overlay_stops_its_two_streams() {
    let mut model = searched("acme", vec![hit(10, "one")]);
    assert_eq!(
        press(&mut model, Key::Esc),
        vec![
            Cmd::CancelStream {
                which: Stream::Search
            },
            Cmd::CancelStream {
                which: Stream::Explain
            },
        ]
    );
}

#[test]
fn emptying_the_search_box_stops_the_running_stream() {
    // Issuing nothing supersedes nothing: without an explicit cancel, the
    // last non-empty query would keep streaming into a box that is now blank.
    let mut model = loaded();
    press(&mut model, Key::Char('/'));
    keys(&mut model, "a");
    let cmds = press(&mut model, Key::Backspace);
    assert!(
        cmds.contains(&Cmd::CancelStream {
            which: Stream::Search
        }),
        "{cmds:?}"
    );
    assert!(
        search_pane(&model).complete,
        "and an empty box is not 'searching…'"
    );
}

#[test]
fn closing_the_why_panel_stops_the_explanation_it_asked_for() {
    let mut model = searched("acme", vec![hit(10, "one")]);
    press(&mut model, Key::Enter);
    press(&mut model, Key::Char('x'));
    assert_eq!(
        press(&mut model, Key::Char('x')),
        vec![Cmd::CancelStream {
            which: Stream::Explain
        }]
    );
    assert!(!search_pane(&model).explain);
}

#[test]
fn leaving_a_list_overlay_cancels_nothing() {
    // The palette, the outbox and the quick menu have no stream behind them,
    // and emitting a cancel for one would abort whichever unrelated stream
    // happened to own that slot.
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    assert!(press(&mut model, Key::Esc).is_empty());
}

#[test]
fn a_window_too_long_to_be_an_undo_gets_no_toast() {
    // Past `MAX_UNDO_TOAST` it is a scheduled send, and a toast for it would
    // hold a row of the screen and repaint the TUI once a second until it
    // expired. The outbox pane shows those instead.
    let mut model = loaded();
    let cmds = update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![outbox_row(
                1,
                "scheduled",
                Some(1_000 + crate::tui::model::MAX_UNDO_TOAST + 1),
            )]),
        },
    );
    assert!(model.toasts.is_empty());
    assert!(cmds.is_empty(), "and no ticker is started: {cmds:?}");
}

#[test]
fn a_refused_cancel_does_not_blank_the_outbox() {
    let mut model = loaded();
    press(&mut model, Key::Char('O'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![outbox_row(1, "scheduled", None)]),
        },
    );
    press(&mut model, Key::Char('u'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_001,
            result: Err("already claimed by a worker".to_owned()),
        },
    );
    match model.overlay.as_ref() {
        Some(Overlay::Outbox(pane)) => assert_eq!(
            pane.rows.len(),
            1,
            "the listing the user is looking at survives a refused cancel"
        ),
        other => panic!("expected the outbox, found {other:?}"),
    }
    assert!(model.status.contains("already claimed"), "{}", model.status);
}
