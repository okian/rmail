//! Task 90's Report overlay: the pane's own merge rules, and the behaviour a
//! key press produces, driven through `tui::model::update` end to end.
//!
//! Both halves live here rather than split across `tui::model::tests` and a
//! pane module, for the reason `tui::overlays::tests` gives: `tasks.md` names
//! `tui::report` as where this task's proof is found, and a bare nextest
//! filter matches a test's *name*, so a suite split across two module paths
//! would leave half of it unselected by the command that claims to run it.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` and `tui::overlays::tests` take.
#![allow(clippy::panic)]

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use rmail_core::command;
use rmail_core::keymap::Mode;

use super::*;
use crate::tui::model::{
    update, Account, Cmd, Confirmed, Folder, Key, MessageRow, Model, Msg, Overlay, ReportEvent,
    Stream,
};
use crate::tui::view;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn invocation(line: &str) -> command::Invocation {
    match command::parse(line) {
        Ok(command::Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
    }
}

fn columns() -> Vec<ReportColumn> {
    vec![
        ReportColumn::new("setting", 8),
        ReportColumn::new("state", 12),
    ]
}

fn pane() -> ReportPane {
    ReportPane::new(invocation("auth status"), "auth", columns(), 1)
}

fn rows(labels: &[&str]) -> Vec<ReportRow> {
    labels
        .iter()
        .map(|label| ReportRow::new([*label]))
        .collect()
}

/// A model with an account, folders and three messages — what a `:` line is
/// typed on top of.
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

/// Type `line` on the command line and run it.
fn run(model: &mut Model, line: &str) -> Vec<Cmd> {
    press(model, Key::Char(':'));
    keys(model, line);
    press(model, Key::Enter)
}

/// The generation an `AuthStatus` request in `cmds` was stamped with.
///
/// Searched for rather than matched positionally: a `:` line that parses is
/// also recorded in the history, so running one returns a `Cmd::SaveHistory`
/// alongside the request.
fn auth_generation(cmds: &[Cmd]) -> u64 {
    cmds.iter()
        .find_map(|cmd| match cmd {
            Cmd::AuthStatus { generation } => Some(*generation),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected an AuthStatus command, found {cmds:?}"))
}

/// A model with the `:auth status` report open, and the generation its request
/// was issued under.
fn reporting() -> (Model, u64) {
    let mut model = loaded();
    let cmds = run(&mut model, "auth status");
    let generation = auth_generation(&cmds);
    (model, generation)
}

fn open_pane(model: &Model) -> &ReportPane {
    match model.overlay.as_ref() {
        Some(Overlay::Report(pane)) => pane,
        other => panic!("expected the report overlay, found {other:?}"),
    }
}

fn frame(model: &mut Model, generation: u64, fill: ReportFill, rows: Vec<ReportRow>, done: bool) {
    update(
        model,
        Msg::Report {
            generation,
            event: ReportEvent::Frame {
                fill,
                rows,
                complete: done,
            },
        },
    );
}

/// Render `model` and flatten the buffer into one string per row.
fn draw(model: &Model, width: u16, height: u16) -> Vec<String> {
    let mut terminal = match Terminal::new(TestBackend::new(width, height)) {
        Ok(terminal) => terminal,
        Err(error) => panic!("the test backend would not start: {error}"),
    };
    if let Err(error) = terminal.draw(|f| view::render(model, f)) {
        panic!("rendering failed: {error}");
    }
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

fn screen(model: &Model) -> String {
    draw(model, 120, 30).join("\n")
}

/// Which screen column `needle` starts in, counting characters.
///
/// `str::find` answers a *byte* offset, and a row carrying an elided cell
/// carries a three-byte ellipsis with it — so two rows aligned on screen have
/// byte offsets two apart, and an alignment assertion written on `find` fails
/// on exactly the row it exists to check.
fn column_of(line: &str, needle: &str) -> Option<usize> {
    line.find(needle)
        .map(|at| line.get(..at).unwrap_or_default().chars().count())
}

// ---------------------------------------------------------------------------
// the pane's merge rules
// ---------------------------------------------------------------------------

#[test]
fn an_appended_frame_extends_the_rows() {
    let mut pane = pane();
    pane.apply(1, ReportFill::Append, rows(&["a", "b"]), false);
    pane.apply(1, ReportFill::Append, rows(&["c"]), true);
    assert_eq!(
        pane.rows
            .iter()
            .map(|row| row.cells[0].clone())
            .collect::<Vec<_>>(),
        ["a", "b", "c"],
        "an appending stream sends each row once, so nothing is dropped"
    );
    assert!(pane.complete);
}

#[test]
fn a_replacing_frame_swaps_the_rows_wholesale() {
    let mut pane = pane();
    pane.apply(1, ReportFill::Append, rows(&["a", "b", "c"]), false);
    pane.apply(1, ReportFill::Replace, rows(&["z"]), true);
    assert_eq!(
        pane.rows
            .iter()
            .map(|row| row.cells[0].clone())
            .collect::<Vec<_>>(),
        ["z"],
        "a snapshot is the whole current answer; keeping the old rows would \
         show results that are no longer results"
    );
}

#[test]
fn a_frame_from_a_superseded_request_is_dropped() {
    let mut pane = pane();
    pane.apply(1, ReportFill::Append, rows(&["current"]), false);
    pane.restart(2);
    pane.apply(1, ReportFill::Append, rows(&["stale"]), true);
    assert!(
        pane.rows.is_empty(),
        "a frame stamped with the previous generation is data about a request \
         nobody is running"
    );
    assert!(
        !pane.complete,
        "and it must not be able to declare the new request finished either"
    );
}

#[test]
fn a_replacing_frame_from_a_superseded_request_cannot_wipe_the_new_rows() {
    let mut pane = pane();
    pane.restart(2);
    pane.apply(2, ReportFill::Append, rows(&["fresh"]), false);
    pane.apply(1, ReportFill::Replace, rows(&["stale"]), true);
    assert_eq!(
        pane.rows
            .iter()
            .map(|row| row.cells[0].clone())
            .collect::<Vec<_>>(),
        ["fresh"],
        "the generation check has to guard the replacing path as well — that \
         is the one where a stale frame is destructive rather than merely wrong"
    );
}

#[test]
fn appending_stops_at_the_row_cap() {
    let mut pane = pane();
    let many: Vec<ReportRow> = (0..MAX_ROWS + 10)
        .map(|n| ReportRow::new([n.to_string()]))
        .collect();
    pane.apply(1, ReportFill::Append, many, false);
    assert_eq!(pane.rows.len(), MAX_ROWS);
    // And a second frame cannot grow it past the cap either, which is the
    // case a single truncation would miss.
    pane.apply(1, ReportFill::Append, rows(&["one more"]), true);
    assert_eq!(pane.rows.len(), MAX_ROWS);
}

#[test]
fn a_replacing_frame_is_truncated_to_the_row_cap() {
    let mut pane = pane();
    let many: Vec<ReportRow> = (0..MAX_ROWS + 10)
        .map(|n| ReportRow::new([n.to_string()]))
        .collect();
    pane.apply(1, ReportFill::Replace, many, true);
    assert_eq!(pane.rows.len(), MAX_ROWS);
}

#[test]
fn a_shrinking_snapshot_brings_the_cursor_back_inside_the_rows() {
    let mut pane = pane();
    pane.apply(1, ReportFill::Replace, rows(&["a", "b", "c", "d"]), false);
    pane.cursor = 3;
    pane.apply(1, ReportFill::Replace, rows(&["a", "b"]), true);
    assert_eq!(
        pane.cursor, 1,
        "a cursor past the end has to come back inside it"
    );
}

#[test]
fn rows_arriving_underneath_leave_the_cursor_where_it_was() {
    let mut pane = pane();
    pane.apply(1, ReportFill::Append, rows(&["a", "b"]), false);
    pane.cursor = 1;
    pane.apply(1, ReportFill::Append, rows(&["c", "d"]), true);
    assert_eq!(
        pane.cursor, 1,
        "a streamed report must not move the selection under the reader's finger"
    );
}

#[test]
fn a_failure_keeps_the_rows_that_did_arrive() {
    let mut pane = pane();
    pane.apply(1, ReportFill::Append, rows(&["walked INBOX"]), false);
    pane.fail(1, "the stream ended".to_owned());
    assert_eq!(
        pane.rows.len(),
        1,
        "what arrived is true, and blanking it throws it away"
    );
    assert_eq!(pane.error.as_deref(), Some("the stream ended"));
    assert!(pane.complete, "there is no more coming");
}

#[test]
fn a_failure_from_a_superseded_request_is_dropped() {
    let mut pane = pane();
    pane.restart(2);
    pane.fail(1, "the previous run died".to_owned());
    assert!(
        pane.error.is_none(),
        "the run that failed is not the run on screen"
    );
    assert!(!pane.complete);
}

#[test]
fn restart_clears_the_rows_and_keeps_the_cursor() {
    let mut pane = pane();
    pane.apply(1, ReportFill::Append, rows(&["a", "b", "c"]), true);
    pane.cursor = 2;
    pane.fail(1, "stale".to_owned());
    pane.restart(2);
    assert!(
        pane.rows.is_empty(),
        "a re-run answers again rather than adding to the old answer"
    );
    assert!(pane.error.is_none());
    assert!(!pane.complete);
    assert_eq!(pane.generation, 2);
    assert_eq!(
        pane.cursor, 2,
        "a refresh that sent the reader back to row 0 every time would make \
         `r` the wrong key to press"
    );
    // And the clamp happens when the new rows land, not before.
    pane.apply(2, ReportFill::Replace, rows(&["only one"]), true);
    assert_eq!(pane.cursor, 0);
}

#[test]
fn the_highlighted_row_is_the_one_under_the_cursor() {
    let mut pane = pane();
    assert!(pane.row().is_none(), "an empty report highlights nothing");
    pane.apply(1, ReportFill::Append, rows(&["a", "b"]), true);
    pane.cursor = 1;
    assert_eq!(pane.row().map(|row| row.cells[0].as_str()), Some("b"));
}

// ---------------------------------------------------------------------------
// rows: bounded, sanitized, tinted
// ---------------------------------------------------------------------------

#[test]
fn a_cell_is_bounded_when_the_row_is_built() {
    let row = ReportRow::new([&"x".repeat(MAX_CELL * 2)]);
    assert_eq!(
        row.cells[0].chars().count(),
        // The kept characters plus the ellipsis marking the cut, which is
        // `truncate_chars`' convention everywhere else in this crate.
        MAX_CELL + 1,
        "a daemon answering with a megabyte in one field is bounded in the \
         model, not only in the renderer"
    );
    assert!(
        row.cells[0].ends_with('…'),
        "and the cut is marked, never silent"
    );
}

#[test]
fn a_cell_cannot_carry_a_newline_or_a_bidi_override_into_the_grid() {
    let row = ReportRow::new(["one\ntwo\u{202e}three"]);
    assert!(
        !row.cells[0].contains('\n'),
        "a newline in a cell would shear the row it is drawn in"
    );
    assert!(
        !row.cells[0].contains('\u{202e}'),
        "and a bidi override would reorder what the reader sees"
    );
}

#[test]
fn a_short_row_is_padded_and_a_long_one_is_not_drawn_past_its_columns() {
    // Two columns, one cell: the second is blank rather than missing, which is
    // what keeps a partly-filled progress row aligned with a complete one.
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![
            ReportRow::new(["short"]),
            ReportRow::new(["a", "b", "an extra cell no column exists for"]),
        ],
        true,
    );
    let rendered = screen(&model);
    assert!(rendered.contains("short"));
    assert!(
        !rendered.contains("an extra cell no column exists for"),
        "a cell with no column has no width to be drawn at"
    );
}

#[test]
fn every_tone_has_its_own_glyph_so_colour_is_never_the_only_signal() {
    let tones = [
        ReportTone::Plain,
        ReportTone::Muted,
        ReportTone::Ok,
        ReportTone::Warn,
        ReportTone::Bad,
    ];
    let glyphs: std::collections::BTreeSet<&str> = tones.iter().map(|t| t.glyph()).collect();
    assert_eq!(
        glyphs.len(),
        tones.len(),
        "a monochrome terminal and a colour-blind reader see the glyph and \
         nothing else"
    );
    for tone in tones {
        assert_eq!(
            tone.glyph().chars().count(),
            1,
            "every glyph is one cell wide, or the columns after it shift"
        );
    }
}

// ---------------------------------------------------------------------------
// the confirmation gate reads `parity::Command::effect`
// ---------------------------------------------------------------------------

#[test]
fn a_verb_reaching_a_mutating_capability_directly_mutates() {
    assert!(
        mutates(&invocation("auth clear")),
        "ClientAuthService/ClearPassword is Effect::Mutate, and `auth clear` \
         is a capability-only verb — its own field is the only place to read it"
    );
}

#[test]
fn an_action_backed_verb_from_the_registry_mutates() {
    for line in ["message delete", "message archive", "message move"] {
        assert!(
            mutates(&invocation(line)),
            "{line} reaches a mutating capability, so a report row carrying it \
             has to ask before it runs"
        );
    }
}

#[test]
fn a_verb_declaring_an_action_but_no_capability_still_mutates() {
    // The registry cannot produce this: an auto-derived verb's capability is
    // filled in from its action, so every real `message delete` answers `true`
    // from the declared half alone. A verb hand-declared in
    // `command::explicit` fills both fields separately and nothing checks that
    // it filled them consistently — so this is the shape the second read in
    // `mutates` exists for, and the only way to exercise it is to build it.
    let mut invocation = invocation("message delete");
    invocation.capability = None;
    assert!(
        mutates(&invocation),
        "a declaration that named the action and forgot the capability must not \
         be a report row that expunges mail with nothing asked"
    );
}

#[test]
fn a_read_verb_with_no_action_and_no_capability_does_not_mutate() {
    // The other side of the same fixture: stripping both fields has to answer
    // `false`, or the gate would ask about every row and teach people to say
    // yes without reading.
    let mut invocation = invocation("message delete");
    invocation.capability = None;
    invocation.action = None;
    assert!(!mutates(&invocation));
}

#[test]
fn a_read_only_capability_does_not_mutate() {
    assert!(
        !mutates(&invocation("auth status")),
        "AuthStatus reveals no secret and changes nothing"
    );
}

#[test]
fn a_verb_that_reaches_no_capability_at_all_does_not_mutate() {
    for line in ["help", "cursor down", "manual"] {
        assert!(
            !mutates(&invocation(line)),
            "{line} is local to this screen, so there is nothing to confirm"
        );
    }
}

// ---------------------------------------------------------------------------
// opening one
// ---------------------------------------------------------------------------

#[test]
fn a_reporting_verb_opens_the_report_and_asks_the_daemon_for_it() {
    let (model, _) = reporting();
    let pane = open_pane(&model);
    assert_eq!(pane.invocation.verb, ["auth", "status"]);
    assert!(pane.rows.is_empty(), "nothing has arrived yet");
    assert!(!pane.complete);
    assert_eq!(
        pane.columns.len(),
        2,
        "the columns are declared when the report opens, not measured from \
         rows that have not arrived"
    );
}

#[test]
fn a_report_derives_the_menu_mode_so_the_list_keys_come_back() {
    let (mut model, generation) = reporting();
    assert_eq!(model.mode(), Mode::Menu);
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        rows(&["a", "b", "c"]),
        true,
    );
    press(&mut model, Key::Char('j'));
    assert_eq!(open_pane(&model).cursor, 1);
    press(&mut model, Key::Char('G'));
    assert_eq!(open_pane(&model).cursor, 2);
    press(&mut model, Key::Char('k'));
    assert_eq!(open_pane(&model).cursor, 1);
}

#[test]
fn the_command_line_is_gone_once_the_report_is_up() {
    let (model, _) = reporting();
    assert!(
        !matches!(model.overlay, Some(Overlay::Command(_))),
        "every action reads `Model::mode`, and one run against a command line \
         that is still up asks the wrong layer what a key means"
    );
}

#[test]
fn the_status_line_says_how_many_rows_arrived() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        rows(&["a", "b"]),
        true,
    );
    assert!(model.status.contains('2'), "{}", model.status);
    assert!(model.status.contains("re-runs"), "{}", model.status);
}

#[test]
fn an_empty_answer_says_so_rather_than_leaving_the_reader_waiting() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        Vec::new(),
        true,
    );
    assert!(
        model.status.contains("nothing to report"),
        "{}",
        model.status
    );
    assert!(screen(&model).contains("nothing to report"));
}

#[test]
fn a_failed_report_names_the_verb_that_failed() {
    let (mut model, generation) = reporting();
    update(
        &mut model,
        Msg::Report {
            generation,
            event: ReportEvent::Failed("the daemon hung up".to_owned()),
        },
    );
    assert!(model.status.contains("auth status"), "{}", model.status);
    assert!(model.status.contains("hung up"), "{}", model.status);
    assert_eq!(
        open_pane(&model).error.as_deref(),
        Some("the daemon hung up")
    );
}

#[test]
fn a_frame_for_a_report_nobody_has_open_is_ignored() {
    let mut model = loaded();
    frame(&mut model, 1, ReportFill::Append, rows(&["a"]), true);
    assert!(
        model.overlay.is_none(),
        "a late frame does not open a screen"
    );
}

// ---------------------------------------------------------------------------
// leaving one, and re-running it
// ---------------------------------------------------------------------------

#[test]
fn esc_closes_the_report_and_cancels_what_was_feeding_it() {
    let (mut model, _) = reporting();
    let cmds = press(&mut model, Key::Esc);
    assert!(model.overlay.is_none());
    assert_eq!(
        cmds,
        vec![Cmd::CancelStream {
            which: Stream::Report
        }],
        "a stale frame is free to ignore; a stale stream is real work on the \
         daemon nobody is going to read"
    );
}

#[test]
fn q_leaves_a_report_the_same_way_esc_does() {
    let (mut model, _) = reporting();
    let cmds = press(&mut model, Key::Char('q'));
    assert!(model.overlay.is_none());
    assert_eq!(
        cmds,
        vec![Cmd::CancelStream {
            which: Stream::Report
        }]
    );
}

#[test]
fn r_re_runs_the_report_and_a_frame_from_the_previous_run_is_dropped() {
    let (mut model, first) = reporting();
    frame(
        &mut model,
        first,
        ReportFill::Replace,
        rows(&["from the first run"]),
        false,
    );
    assert_eq!(open_pane(&model).rows.len(), 1);

    let cmds = press(&mut model, Key::Char('r'));
    assert!(
        !cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::CancelStream {
                which: Stream::Report
            }
        )),
        "the superseding request is what cancels the old stream — the same rule \
         `restart_search` follows, so `r` must not grow a second mechanism for \
         it: {cmds:?}"
    );
    let second = auth_generation(&cmds);
    assert_ne!(
        second, first,
        "a re-run is a new generation, or its own tail lands in it"
    );
    assert!(
        open_pane(&model).rows.is_empty(),
        "the previous answer is cleared"
    );
    assert_eq!(
        open_pane(&model).invocation.verb,
        ["auth", "status"],
        "the pane re-runs its own stored invocation"
    );

    // The mid-flight frame from the run that was superseded.
    frame(
        &mut model,
        first,
        ReportFill::Replace,
        rows(&["stale"]),
        true,
    );
    assert!(
        open_pane(&model).rows.is_empty(),
        "{:?}",
        open_pane(&model).rows
    );
    assert!(!open_pane(&model).complete);

    frame(
        &mut model,
        second,
        ReportFill::Replace,
        rows(&["from the second run"]),
        true,
    );
    assert_eq!(
        open_pane(&model).rows[0].cells[0],
        "from the second run",
        "and the current run's frame lands"
    );
}

#[test]
fn r_in_a_menu_that_is_not_a_report_does_nothing() {
    // `r` is `message.reply` in Normal and `report.rerun` in Menu; bound in
    // the layer, it has to be inert on the other panes that share it rather
    // than reaching past them.
    let mut model = loaded();
    press(&mut model, Key::Char('O'));
    let overlay_before = model.overlay.clone();
    let cmds = press(&mut model, Key::Char('r'));
    assert!(cmds.is_empty());
    assert_eq!(model.overlay, overlay_before);
}

#[test]
fn colon_over_a_report_replaces_it_and_cancels_its_stream() {
    // `:` is bound in `Menu` as well as `Normal` (task 89), and a command line
    // opened over a report has to stop the stream that report was reading.
    let (mut model, _) = reporting();
    let cmds = press(&mut model, Key::Char(':'));
    assert!(matches!(model.overlay, Some(Overlay::Command(_))));
    assert_eq!(
        cmds,
        vec![Cmd::CancelStream {
            which: Stream::Report
        }]
    );
}

// ---------------------------------------------------------------------------
// running a row
// ---------------------------------------------------------------------------

#[test]
fn enter_on_a_mutating_row_asks_first_and_y_runs_it() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![ReportRow::new(["password", "configured"]).running(invocation("auth clear"))],
        true,
    );
    let cmds = press(&mut model, Key::Enter);
    assert!(
        cmds.is_empty(),
        "nothing is sent until the question is answered"
    );
    match model.overlay.as_ref() {
        Some(Overlay::Confirm { prompt, then }) => {
            assert!(prompt.contains("auth clear"), "{prompt}");
            match then {
                Confirmed::Invoke { invocation, over } => {
                    assert_eq!(invocation.verb, ["auth", "clear"]);
                    assert!(
                        invocation.bang,
                        "the gate stamps the bang so an answered question is \
                         not asked again"
                    );
                    assert_eq!(
                        over.as_ref().map(|over| over.invocation.verb.clone()),
                        Some(vec!["auth".to_owned(), "status".to_owned()]),
                        "and the question carries the report it was asked over"
                    );
                }
                other => panic!("expected an invocation to run, found {other:?}"),
            }
        }
        other => panic!("expected a confirmation, found {other:?}"),
    }
    assert_eq!(model.mode(), Mode::Confirm);
    let cmds = press(&mut model, Key::Char('y'));
    assert_eq!(cmds, vec![Cmd::AuthClear]);
    let pane = open_pane(&model);
    assert_eq!(
        pane.invocation.verb,
        ["auth", "status"],
        "the reader is left on the screen the row was on, not two layers up"
    );
    assert!(
        pane.stale,
        "and told that what it shows is from before the row ran"
    );
}

#[test]
fn declining_the_confirmation_runs_nothing() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![ReportRow::new(["password"]).running(invocation("auth clear"))],
        true,
    );
    press(&mut model, Key::Enter);
    let cmds = press(&mut model, Key::Char('n'));
    assert!(cmds.is_empty());
    let pane = open_pane(&model);
    assert!(
        !pane.stale,
        "declining ran nothing, so nothing about the rows changed"
    );
    assert_eq!(
        pane.rows.len(),
        1,
        "and the report is back with the rows it had"
    );
}

#[test]
fn enter_on_a_read_only_row_runs_it_without_asking() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![ReportRow::new(["refresh"]).running(invocation("auth status"))],
        true,
    );
    let cmds = press(&mut model, Key::Enter);
    assert!(
        matches!(cmds.as_slice(), [Cmd::AuthStatus { .. }]),
        "a row that only reads is not a question, and asking about one \
         teaches people to answer yes without looking: {cmds:?}"
    );
    assert!(matches!(model.overlay, Some(Overlay::Report(_))));
}

#[test]
fn a_re_run_clears_the_stale_marking_because_the_rows_are_fresh_again() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![ReportRow::new(["password"]).running(invocation("auth clear!"))],
        true,
    );
    press(&mut model, Key::Enter);
    assert!(open_pane(&model).stale);
    press(&mut model, Key::Char('r'));
    assert!(
        !open_pane(&model).stale,
        "`r` is what re-reads, so it is what un-stales"
    );
}

#[test]
fn a_read_only_row_does_not_mark_the_report_stale() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![ReportRow::new(["refresh"]).running(invocation("auth status"))],
        true,
    );
    press(&mut model, Key::Enter);
    assert!(
        !open_pane(&model).stale,
        "nothing changed, so nothing on screen is out of date"
    );
}

#[test]
fn a_row_carrying_a_verb_this_screen_cannot_run_says_so_on_the_status_line() {
    // The row's command is dispatched with the report down and no command line
    // ever opened, so a refusal written only into the command pane would be a
    // keystroke that silently did nothing.
    let (mut model, generation) = reporting();
    // A *read* verb, so the row runs rather than asking first — the refusal is
    // what this is about, and a confirmation would arrive before it.
    let mut takes_no_arguments = invocation("auth status");
    takes_no_arguments.positionals = vec!["now".to_owned()];
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![ReportRow::new(["odd row"]).running(takes_no_arguments)],
        true,
    );
    let cmds = press(&mut model, Key::Enter);
    assert!(cmds.is_empty());
    assert!(
        model.status.contains("takes no arguments"),
        "{}",
        model.status
    );
    assert!(
        matches!(model.overlay, Some(Overlay::Report(_))),
        "and the report the row was on is still there to read the complaint from"
    );
}

#[test]
fn enter_on_an_informational_row_does_nothing() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        rows(&["just a fact"]),
        true,
    );
    let cmds = press(&mut model, Key::Enter);
    assert!(cmds.is_empty());
    assert!(
        matches!(model.overlay, Some(Overlay::Report(_))),
        "and it does not close the report either"
    );
}

#[test]
fn a_row_whose_invocation_already_carries_a_bang_is_not_asked_about() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![ReportRow::new(["password"]).running(invocation("auth clear!"))],
        true,
    );
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::AuthClear],
        "`!` means skip the question wherever it appears, and a row is not an \
         exception to that"
    );
}

#[test]
fn a_mutating_row_that_opens_its_own_confirmation_is_not_asked_about_twice() {
    // `message delete` opens a `Confirm` of its own, and the gate's bang is
    // what stops the confirmed row from asking again. One implementation of
    // "skip the question", exercised end to end.
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![ReportRow::new(["delete it"]).running(invocation("message delete"))],
        true,
    );
    press(&mut model, Key::Enter);
    let cmds = press(&mut model, Key::Char('y'));
    assert_eq!(
        cmds,
        vec![Cmd::Delete { message_id: 10 }],
        "one question, then the work: {cmds:?}"
    );
    assert!(
        matches!(model.overlay, Some(Overlay::Report(_))),
        "and the report is back rather than a second question: {:?}",
        model.overlay
    );
}

// ---------------------------------------------------------------------------
// `d` still means what it did
// ---------------------------------------------------------------------------

#[test]
fn the_delete_confirmation_still_carries_the_messages_it_captured() {
    // `Overlay::Confirm` grew a second thing it can mean; the first must not
    // have changed. The ids are captured when the question is asked, so a
    // reload arriving while it is up cannot move the target.
    let mut model = loaded();
    press(&mut model, Key::Char('d'));
    match model.overlay.as_ref() {
        Some(Overlay::Confirm {
            then: Confirmed::Delete(ids),
            ..
        }) => assert_eq!(ids, &[10]),
        other => panic!("expected a delete confirmation, found {other:?}"),
    }
    let cmds = press(&mut model, Key::Char('y'));
    assert_eq!(cmds, vec![Cmd::Delete { message_id: 10 }]);
}

// ---------------------------------------------------------------------------
// drawing one
// ---------------------------------------------------------------------------

#[test]
fn a_report_draws_its_headers_and_pads_its_cells_to_the_declared_widths() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![
            ReportRow::new(["password", "configured"]),
            ReportRow::new(["local login", "not required"]),
        ],
        true,
    );
    let rendered = draw(&model, 120, 30);
    let header = rendered
        .iter()
        .find(|line| line.contains("setting") && line.contains("state"))
        .cloned()
        .unwrap_or_default();
    let first = rendered
        .iter()
        .find(|line| line.contains("password"))
        .cloned()
        .unwrap_or_default();
    let second = rendered
        .iter()
        .find(|line| line.contains("local login"))
        .cloned()
        .unwrap_or_default();
    // The second column starts in the same screen column on all three rows,
    // which is the whole claim "fixed-width columns" makes.
    assert_eq!(
        column_of(&header, "state"),
        column_of(&first, "configured"),
        "header {header:?} / row {first:?}"
    );
    assert_eq!(
        column_of(&first, "configured"),
        column_of(&second, "not required"),
        "row {first:?} / row {second:?}"
    );
}

#[test]
fn a_cell_wider_than_its_column_is_elided_rather_than_pushing_the_next_one_over() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![
            ReportRow::new(["short", "SECOND"]),
            ReportRow::new([&"w".repeat(200), "SECOND"]),
        ],
        true,
    );
    let rendered = draw(&model, 120, 30);
    let matched: Vec<&String> = rendered
        .iter()
        .filter(|line| line.contains("SECOND"))
        .collect();
    assert_eq!(
        matched.len(),
        2,
        "both rows drew their second cell: {rendered:#?}"
    );
    assert_eq!(
        column_of(matched[0], "SECOND"),
        column_of(matched[1], "SECOND"),
        "and drew it in the same column:\n{}\n{}",
        matched[0],
        matched[1]
    );
}

#[test]
fn a_row_that_runs_something_is_marked_and_one_that_does_not_is_not() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![
            ReportRow::new(["actionable"]).running(invocation("auth clear")),
            ReportRow::new(["informational"]),
        ],
        true,
    );
    let rendered = draw(&model, 120, 30);
    let marked = rendered
        .iter()
        .find(|line| line.contains("actionable"))
        .cloned()
        .unwrap_or_default();
    let plain = rendered
        .iter()
        .find(|line| line.contains("informational"))
        .cloned()
        .unwrap_or_default();
    assert!(
        marked.contains('⏎'),
        "a reader has to be able to tell Enter will do something: {marked:?}"
    );
    assert!(!plain.contains('⏎'), "{plain:?}");
}

#[test]
fn a_tone_draws_its_glyph() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![
            ReportRow::new(["healthy"]).toned(ReportTone::Ok),
            ReportRow::new(["broken"]).toned(ReportTone::Bad),
        ],
        true,
    );
    let rendered = draw(&model, 120, 30);
    for (needle, glyph) in [("healthy", '✓'), ("broken", '✗')] {
        let line = rendered
            .iter()
            .find(|line| line.contains(needle))
            .cloned()
            .unwrap_or_default();
        assert!(line.contains(glyph), "{needle}: {line:?}");
    }
}

#[test]
fn a_stale_report_says_so_in_its_own_title() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        vec![ReportRow::new(["password"]).running(invocation("auth clear!"))],
        true,
    );
    press(&mut model, Key::Enter);
    let rendered = screen(&model);
    assert!(
        rendered.contains("stale"),
        "a report describing how things were, without saying so, is a wrong \
         answer with no marking: {rendered}"
    );
}

#[test]
fn a_report_says_it_is_asking_until_the_first_frame_lands() {
    let (mut model, generation) = reporting();
    assert!(screen(&model).contains("asking"));
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        rows(&["a"]),
        true,
    );
    let rendered = screen(&model);
    assert!(!rendered.contains("asking"), "{rendered}");
    assert!(rendered.contains("re-runs"));
}

#[test]
fn a_report_that_failed_with_nothing_to_show_draws_the_error() {
    let (mut model, generation) = reporting();
    update(
        &mut model,
        Msg::Report {
            generation,
            event: ReportEvent::Failed("UNAVAILABLE: no daemon".to_owned()),
        },
    );
    let rendered = screen(&model);
    assert!(rendered.contains("no daemon"), "{rendered}");
    assert!(rendered.contains("failed"), "{rendered}");
}

#[test]
fn a_report_survives_a_terminal_too_small_to_draw_it() {
    let (mut model, generation) = reporting();
    frame(
        &mut model,
        generation,
        ReportFill::Replace,
        rows(&["a", "b"]),
        true,
    );
    // No assertion beyond "this returns": ratatui's layout arithmetic panics
    // on a zero-width rectangle, and every overlay here is expected to clamp
    // rather than to be given a terminal that fits.
    assert_eq!(draw(&model, 8, 4).len(), 4);
}
