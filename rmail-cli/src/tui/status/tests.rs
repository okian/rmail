//! Task 92's status bar and heartbeat: every mode labelled, every indicator
//! state distinguishable, and a poll that never touches the busy marker.
//!
//! Driven through `tui::model::update` where the question is about state, and
//! through `tui::view` against ratatui's `TestBackend` where it is about zones —
//! which several of these genuinely are, since "the mode is in the same columns
//! whether or not anything failed" is a claim about drawing and nothing else.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use std::collections::BTreeSet;

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use rmail_core::keymap::Key;

use super::*;
use crate::tui::model::{
    update, Account, Cmd, Folder, InputFor, Level, MessageRow, Msg, OpenMessage, Overlay, Screen,
    SEEN,
};
use crate::tui::view;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn row(id: i64, seen: bool) -> MessageRow {
    MessageRow {
        id,
        subject: format!("subject {id}"),
        from: "Alice".to_owned(),
        from_addr: Some("alice@example.com".to_owned()),
        date: Some(1_700_000_000 + id),
        flags: if seen {
            vec![SEEN.to_owned()]
        } else {
            Vec::new()
        },
        has_attachments: false,
    }
}

fn loaded() -> Model {
    let mut model = Model::new();
    model.account = Some(Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    });
    model.folders = vec![
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
    ];
    model.open_folder = Some(1);
    model.messages = vec![row(10, false), row(11, true), row(12, true)];
    model
}

fn press(model: &mut Model, key: Key) -> Vec<Cmd> {
    update(model, Msg::Key(key))
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

/// The status row — the last one on screen.
fn status_row(model: &Model, width: u16) -> String {
    draw(model, width, 24).last().cloned().unwrap_or_default()
}

/// Which screen column `needle` starts in, counting characters.
///
/// `str::find` answers a byte offset, and the indicator glyphs are multi-byte —
/// so a column assertion written on `find` would compare the wrong numbers.
fn column_of(line: &str, needle: &str) -> Option<usize> {
    line.find(needle)
        .map(|at| line.get(..at).unwrap_or_default().chars().count())
}

/// Every mode the model can actually derive, with a model in that state.
fn every_reachable_mode() -> Vec<(Mode, Model)> {
    let mut out = Vec::new();

    out.push((Mode::Normal, loaded()));

    let mut viewer = loaded();
    viewer.screen = Screen::Viewer;
    viewer.open = Some(OpenMessage {
        id: 10,
        headers: vec![("Subject".to_owned(), "subject 10".to_owned())],
        body: vec!["body".to_owned()],
        has_html: false,
        attachments: Vec::new(),
    });
    out.push((Mode::Viewer, viewer));

    let mut visual = loaded();
    press(&mut visual, Key::Char('v'));
    out.push((Mode::Visual, visual));

    let mut insert = loaded();
    insert.overlay = Some(Overlay::Input {
        prompt: "forward to".to_owned(),
        buffer: String::new(),
        what: InputFor::ForwardTo,
        message_id: 10,
    });
    out.push((Mode::Insert, insert));

    let mut prompt = loaded();
    press(&mut prompt, Key::Char('/'));
    out.push((Mode::Prompt, prompt));

    let mut menu = loaded();
    press(&mut menu, Key::Char('O'));
    out.push((Mode::Menu, menu));

    let mut pick = loaded();
    press(&mut pick, Key::Char('c'));
    out.push((Mode::Pick, pick));

    let mut confirm = loaded();
    press(&mut confirm, Key::Char('d'));
    out.push((Mode::Confirm, confirm));

    let mut help = loaded();
    press(&mut help, Key::Char('?'));
    out.push((Mode::Help, help));

    out
}

// ---------------------------------------------------------------------------
// all ten modes
// ---------------------------------------------------------------------------

#[test]
fn every_mode_has_its_own_label() {
    let modes: Vec<Mode> = Mode::CONFIGURABLE
        .iter()
        .copied()
        .chain(std::iter::once(Mode::Global))
        .collect();
    assert_eq!(
        modes.len(),
        10,
        "ten layers, and the bar labels all of them"
    );
    let labels: BTreeSet<String> = modes.iter().copied().map(mode_label).collect();
    assert_eq!(
        labels.len(),
        modes.len(),
        "no two layers share a label: {labels:?}"
    );
    for mode in modes {
        let label = mode_label(mode);
        assert!(
            label.to_lowercase().contains(mode.id()),
            "{label:?} does not name {}",
            mode.id()
        );
        assert!(
            label.chars().count() <= MODE_WIDTH,
            "{label:?} does not fit the zone's {MODE_WIDTH} columns"
        );
    }
}

#[test]
fn every_reachable_mode_draws_its_label_on_the_bar() {
    // Nine of the ten: `Mode::Global` is never active on its own, which is why
    // it is absent from `Mode::CONFIGURABLE` too. It is still labelled, because
    // `mode_label` is total over the enum and a mode a later task promotes
    // should not need an edit here.
    for (mode, model) in every_reachable_mode() {
        assert_eq!(
            model.mode(),
            mode,
            "the fixture for {} does not derive it",
            mode.id()
        );
        let row = status_row(&model, 120);
        assert!(
            row.contains(&mode_label(mode)),
            "{} is not on the bar: {row:?}",
            mode.id()
        );
    }
}

#[test]
fn prompt_and_insert_are_no_longer_labelled_the_same() {
    // Task 83 drew both as `-- INSERT --`. They are different layers with
    // different bindings, and the bar saying which one is live is how somebody
    // works out why `<tab>` did something unexpected.
    assert_ne!(mode_label(Mode::Insert), mode_label(Mode::Prompt));
}

// ---------------------------------------------------------------------------
// zones
// ---------------------------------------------------------------------------

#[test]
fn a_long_message_does_not_push_the_fixed_zones_off_the_row() {
    let mut model = loaded();
    let narrow = status_row(&model, 120);
    let mode_at = column_of(&narrow, "-- NORMAL --");

    model.status = "x".repeat(400);
    model.level = Level::Error;
    let wide = status_row(&model, 120);
    assert_eq!(
        column_of(&wide, "-- NORMAL --"),
        mode_at,
        "the mode is in the same columns whether or not anything failed:\\n{narrow}\\n{wide}"
    );
    assert!(
        wide.contains("sync") || wide.contains("idx"),
        "and the daemon zone survives a four-hundred-character rejection: {wide}"
    );
}

#[test]
fn the_scope_zone_names_the_account_the_open_folder_and_the_unread_rows() {
    let model = loaded();
    let bar = bar(&model);
    assert_eq!(
        bar.scope, "personal/INBOX 1▾",
        "one of the three loaded rows is unread"
    );
}

#[test]
fn the_scope_zone_follows_the_open_folder_not_the_folder_cursor() {
    // Moving the folder cursor must not repaint the zone with another folder's
    // name over the open folder's unread count.
    let mut model = loaded();
    model.folder_idx = 1;
    assert_eq!(
        model.current_folder().map(|folder| folder.name.as_str()),
        Some("Archive")
    );
    assert_eq!(bar(&model).scope, "personal/INBOX 1▾");
}

#[test]
fn the_scope_zone_says_nothing_about_unread_when_everything_is_read() {
    let mut model = loaded();
    model.messages = vec![row(10, true), row(11, true)];
    assert_eq!(bar(&model).scope, "personal/INBOX");
}

#[test]
fn the_busy_marker_is_absent_rather_than_zero() {
    let mut model = loaded();
    assert!(
        bar(&model).inflight.is_empty(),
        "a permanent `0 in flight` is a permanent claim that nothing is happening"
    );
    model.inflight = 3;
    assert!(bar(&model).inflight.contains('3'));
}

#[test]
fn the_pending_zone_shows_the_count_as_well_as_the_keys() {
    let mut model = loaded();
    press(&mut model, Key::Char('3'));
    assert_eq!(
        bar(&model).pending,
        "3",
        "a count alone is still half-typed"
    );
    press(&mut model, Key::Char('g'));
    assert_eq!(bar(&model).pending, "3g");
    let row = status_row(&model, 120);
    assert!(row.contains("3g"), "{row}");
}

#[test]
fn the_daemon_zone_is_dropped_before_the_message_is_squeezed_to_nothing() {
    let mut model = loaded();
    model.status = "something went wrong and here is why".to_owned();
    let wide = status_row(&model, 120);
    assert!(wide.contains("sync"), "{wide}");
    let narrow = status_row(&model, 46);
    assert!(
        !narrow.contains("sync"),
        "the indicators go before the sentence explaining why one of them is \\
         red: {narrow}"
    );
    assert!(
        narrow.contains("something went wrong"),
        "and the message survives: {narrow}"
    );
    assert!(
        narrow.contains("-- NORMAL --"),
        "as does the mode: {narrow}"
    );
}

#[test]
fn the_bar_survives_a_terminal_narrower_than_its_undroppable_zones() {
    let model = loaded();
    // No assertion beyond "this returns": `Layout` truncates, and there is no
    // useful answer at this width — shuffling which fact survives would make
    // the bar unreadable rather than merely cramped.
    assert_eq!(draw(&model, 8, 4).len(), 4);
}

// ---------------------------------------------------------------------------
// indicators
// ---------------------------------------------------------------------------

#[test]
fn every_state_has_its_own_single_width_glyph() {
    let states = [
        HealthState::Unknown,
        HealthState::Ok,
        HealthState::Busy,
        HealthState::Paused,
        HealthState::Off,
        HealthState::Strained,
        HealthState::Failed,
    ];
    let glyphs: BTreeSet<&str> = states.iter().map(|state| state.glyph()).collect();
    assert_eq!(
        glyphs.len(),
        states.len(),
        "a duplicate glyph makes the colour load-bearing again: {glyphs:?}"
    );
    for state in states {
        assert_eq!(
            state.glyph().chars().count(),
            1,
            "{state:?}'s glyph is not one cell, so the zone after it would shift"
        );
    }
}

#[test]
fn the_states_that_want_attention_are_the_ones_that_read_as_warnings() {
    use crate::tui::report::ReportTone;
    assert_eq!(HealthState::Ok.tone(), ReportTone::Ok);
    assert_eq!(HealthState::Failed.tone(), ReportTone::Bad);
    for state in [HealthState::Paused, HealthState::Strained] {
        assert_eq!(state.tone(), ReportTone::Warn, "{state:?}");
    }
    for state in [HealthState::Busy, HealthState::Off] {
        assert_eq!(
            state.tone(),
            ReportTone::Muted,
            "{state:?} is a fact about configuration or progress, not a fault"
        );
    }
    assert_eq!(
        HealthState::Unknown.tone(),
        ReportTone::Plain,
        "nobody has answered yet, which is neither good nor bad"
    );
}

#[test]
fn an_indicator_starts_unknown_and_draws_its_glyph() {
    let model = loaded();
    let bar = bar(&model);
    assert_eq!(bar.daemon.len(), Subsystem::ALL.len());
    for indicator in &bar.daemon {
        assert_eq!(
            indicator.state,
            HealthState::Unknown,
            "{:?} claims to know something before the heartbeat has answered",
            indicator.which
        );
    }
    let row = status_row(&model, 120);
    assert!(row.contains("?sync"), "{row}");
}

#[test]
fn a_heartbeat_answer_reaches_the_indicator_it_is_about() {
    let mut model = loaded();
    update(
        &mut model,
        Msg::Daemon {
            subsystem: Subsystem::Index,
            result: Ok(Health::new(HealthState::Busy, "queue 4")),
        },
    );
    assert_eq!(model.daemon.index.state, HealthState::Busy);
    assert_eq!(model.daemon.index.detail, "queue 4");
    assert_eq!(
        model.daemon.sync.state,
        HealthState::Unknown,
        "and only the one it is about"
    );
    let row = status_row(&model, 120);
    assert!(row.contains("↻idx"), "{row}");
}

#[test]
fn a_heartbeat_that_could_not_be_asked_marks_the_indicator_and_leaves_the_status_line_alone() {
    let mut model = loaded();
    model.status = "3 messages".to_owned();
    model.level = Level::Info;
    update(
        &mut model,
        Msg::Daemon {
            subsystem: Subsystem::Ai,
            result: Err("UNAVAILABLE: no daemon".to_owned()),
        },
    );
    assert_eq!(model.daemon.ai.state, HealthState::Failed);
    assert!(model.daemon.ai.detail.contains("no daemon"));
    assert_eq!(
        model.status, "3 messages",
        "nobody asked for the heartbeat, so it must not overwrite the answer \\
         to whatever the user did ask — once every five seconds, forever"
    );
}

#[test]
fn a_heartbeat_never_touches_the_inflight_count() {
    // The claim the acceptance singles out: a poll that incremented `inflight`
    // would pin the busy marker on forever and destroy the one signal it
    // carries; one that decremented would drive it below zero on the first
    // tick.
    let mut model = loaded();
    model.inflight = 2;
    for subsystem in Subsystem::ALL {
        update(
            &mut model,
            Msg::Daemon {
                subsystem: *subsystem,
                result: Ok(Health::new(HealthState::Ok, "fine")),
            },
        );
        update(
            &mut model,
            Msg::Daemon {
                subsystem: *subsystem,
                result: Err("gone".to_owned()),
            },
        );
    }
    assert_eq!(model.inflight, 2, "in neither direction");
}

#[test]
fn the_heartbeat_starts_once_the_account_is_known_and_is_not_counted() {
    let mut model = Model::new();
    let cmds = update(
        &mut model,
        Msg::Accounts(Ok(vec![Account {
            id: 7,
            name: "personal".to_owned(),
            username: None,
        }])),
    );
    assert!(
        cmds.contains(&Cmd::Heartbeat { account_id: 7 }),
        "the heartbeat needs the account for `SyncService.Status` and \\
         `GetSpend`: {cmds:?}"
    );
    assert_eq!(
        model.inflight, 2,
        "the folder and outbox listings are counted; the heartbeat and the \\
         event stream are not, because nobody asked for either and neither \\
         finishes"
    );
}

#[test]
fn a_folder_listing_does_not_reset_what_the_same_rpc_just_reported() {
    // `Cmd::LoadFolders`' RPC *is* the sync indicator's own, so the executor
    // reports both from one call (`grpc::tests::a_folder_listing_also_reports_
    // the_sync_indicator`) and a reload preempts the next tick. This is the
    // model half: a `Msg::Daemon` arriving between ticks lands, and the folder
    // listing that arrives with it does not undo it.
    let mut model = loaded();
    update(
        &mut model,
        Msg::Daemon {
            subsystem: Subsystem::Sync,
            result: Ok(Health::new(HealthState::Paused, "paused · 2 folder(s)")),
        },
    );
    let folders = model.folders.clone();
    update(&mut model, Msg::Folders(Ok(folders)));
    assert_eq!(
        model.daemon.sync.state,
        HealthState::Paused,
        "the folder listing must not reset what the same RPC just reported"
    );
}

// ---------------------------------------------------------------------------
// proto to state: what each indicator actually reads
// ---------------------------------------------------------------------------

use crate::tui::model::wire;
use rmail_proto::v1::{
    BudgetCaps, BudgetSpend, BudgetWindowCaps, ClassSpend, DayUsage, FolderStatus,
    GetSpendResponse, IndexStatusResponse, QueueStats, SyncStatusResponse, UsageStats,
};

fn folder(id: i64) -> FolderStatus {
    FolderStatus {
        mailbox_id: id,
        name: format!("folder {id}"),
        ..FolderStatus::default()
    }
}

#[test]
fn sync_reads_paused_as_a_choice_rather_than_a_fault() {
    let running = wire::sync_health(&SyncStatusResponse {
        folders: vec![folder(1), folder(2)],
        paused: false,
    });
    assert_eq!(running.state, HealthState::Ok);
    assert!(running.detail.contains('2'), "{running:?}");

    let paused = wire::sync_health(&SyncStatusResponse {
        folders: vec![folder(1)],
        paused: true,
    });
    assert_eq!(
        paused.state,
        HealthState::Paused,
        "an operator paused it, so it is a warning and not a failure"
    );
    assert_eq!(paused.state.tone(), crate::tui::report::ReportTone::Warn);
}

#[test]
fn the_index_reports_quarantined_jobs_ahead_of_a_pause() {
    let base = IndexStatusResponse {
        messages: 40,
        ..IndexStatusResponse::default()
    };
    assert_eq!(wire::index_health(&base).state, HealthState::Ok);

    let working = IndexStatusResponse {
        queue_ready: 3,
        ..base.clone()
    };
    assert_eq!(wire::index_health(&working).state, HealthState::Busy);

    let paused = IndexStatusResponse {
        paused: true,
        queue_ready: 3,
        ..base.clone()
    };
    assert_eq!(wire::index_health(&paused).state, HealthState::Paused);

    // Both at once: a quarantined job is work that will never happen without
    // somebody looking, and a paused worker is already waiting for exactly
    // that. Reporting the pause and hiding the dead jobs hides the reason to
    // look.
    let dead = IndexStatusResponse {
        paused: true,
        queue_dead: 2,
        ..base
    };
    let dead = wire::index_health(&dead);
    assert_eq!(dead.state, HealthState::Strained);
    assert!(dead.detail.contains("quarantined"), "{dead:?}");
}

#[test]
fn ai_never_reports_a_disabled_subsystem_as_running() {
    // The trap the proto warns about in as many words: a daemon with
    // `ai.enabled = false` never spawns the dispatch loop, so `paused` stays
    // false. Reading that as "running" would send somebody to resume something
    // no RPC can start — and `HealthState::Off` exists so it cannot.
    let disabled = wire::ai_health(&UsageStats {
        enabled: false,
        paused: false,
        ..UsageStats::default()
    });
    assert_eq!(disabled.state, HealthState::Off);
    assert!(disabled.detail.contains("config"), "{disabled:?}");

    let running = wire::ai_health(&UsageStats {
        enabled: true,
        today: Some(DayUsage {
            cost_usd: 0.125,
            ..DayUsage::default()
        }),
        ..UsageStats::default()
    });
    assert_eq!(running.state, HealthState::Ok);
    assert!(
        running.detail.contains("$0.13") || running.detail.contains("$0.12"),
        "spend is reported at cent precision, not the provider's own: {running:?}"
    );

    let busy = wire::ai_health(&UsageStats {
        enabled: true,
        queue: Some(QueueStats {
            leased: 2,
            ..QueueStats::default()
        }),
        ..UsageStats::default()
    });
    assert_eq!(busy.state, HealthState::Busy);

    let dead = wire::ai_health(&UsageStats {
        enabled: true,
        paused: true,
        queue: Some(QueueStats {
            dead: 1,
            ..QueueStats::default()
        }),
        ..UsageStats::default()
    });
    assert_eq!(
        dead.state,
        HealthState::Strained,
        "dead outranks paused here too"
    );
}

fn spend(usd: f64, soft: Option<f64>, hard: Option<f64>) -> GetSpendResponse {
    GetSpendResponse {
        all: Some(ClassSpend {
            daily: Some(BudgetSpend { usd, tokens: 0 }),
            caps: Some(BudgetCaps {
                daily: Some(BudgetWindowCaps {
                    soft_usd: soft,
                    hard_usd: hard,
                    ..BudgetWindowCaps::default()
                }),
                monthly: None,
            }),
            ..ClassSpend::default()
        }),
        ..GetSpendResponse::default()
    }
}

#[test]
fn spend_is_measured_against_the_hard_cap_first() {
    // At or above the hard cap the daemon blocks dispatch, which is a fault and
    // has to read as one rather than as "nearly there".
    let blocked = wire::spend_health(&spend(5.0, Some(2.0), Some(5.0)));
    assert_eq!(blocked.state, HealthState::Failed);
    assert!(blocked.detail.contains("blocked"), "{blocked:?}");

    let downgrading = wire::spend_health(&spend(3.0, Some(2.0), Some(5.0)));
    assert_eq!(downgrading.state, HealthState::Strained);
    assert!(
        downgrading.detail.contains("downgrading"),
        "{downgrading:?}"
    );

    let fine = wire::spend_health(&spend(0.5, Some(2.0), Some(5.0)));
    assert_eq!(fine.state, HealthState::Ok);
    assert!(fine.detail.contains("$5.00"), "{fine:?}");
}

#[test]
fn an_uncapped_scope_is_not_a_warning() {
    // Unlimited is a configuration, not a fault. Drawing it as one would make
    // the zone permanently yellow on a default install, which is how a bar
    // teaches people to stop reading it.
    let health = wire::spend_health(&spend(1.25, None, None));
    assert_eq!(health.state, HealthState::Ok);
    assert!(health.detail.contains("no cap"), "{health:?}");
    assert!(health.detail.contains("$1.25"), "{health:?}");
}

#[test]
fn a_scope_reporting_no_spend_at_all_is_unknown_rather_than_zero() {
    let health = wire::spend_health(&GetSpendResponse::default());
    assert_eq!(
        health.state,
        HealthState::Unknown,
        "`$0.00 today` would be a claim, and this response makes none"
    );
}

// ---------------------------------------------------------------------------
// the `:` command an indicator expands into
// ---------------------------------------------------------------------------

#[test]
fn an_indicator_names_its_command_only_when_this_build_has_it() {
    // Tasks 94 and 96 declare `:index status` and `:ai budget status`. Naming a
    // command that does not resolve would be the bar telling somebody to type
    // something that answers "unknown command"; declaring the verbs there turns
    // these hints on with no edit here.
    let model = loaded();
    for indicator in &bar(&model).daemon {
        let path = indicator.which.verb();
        let declared = rmail_core::command::verb_at(path).is_some();
        assert_eq!(
            indicator.expands.is_some(),
            declared,
            "{:?} claims `{}` is {}available",
            indicator.which,
            path.join(" "),
            if declared { "un" } else { "" }
        );
    }
}

#[test]
fn the_hint_is_the_verbs_own_canonical_spelling() {
    // Task 94 declared three of the four, so those hints now name one — which
    // is the derivation working rather than the hints being written down:
    // nothing in `tui::status` changed when those verbs arrived.
    let model = loaded();
    let named: Vec<Option<String>> = bar(&model)
        .daemon
        .iter()
        .map(|indicator| indicator.expands.clone())
        .collect();
    assert_eq!(
        named,
        [
            Some(":sync status".to_owned()),
            Some(":index status".to_owned()),
            Some(":ai status".to_owned()),
            // Task 96's, and still undeclared — which is what keeps the
            // `Option` load-bearing rather than decorative.
            None,
        ],
        "each hint is the verb's own canonical spelling, in indicator order"
    );
}

// ---------------------------------------------------------------------------
// the focus hint stays task 93's
// ---------------------------------------------------------------------------

#[test]
fn the_focus_hint_is_eligible_on_the_folder_pane_and_shown_only_when_it_is_hidden() {
    let mut model = loaded();
    assert!(
        bar(&model).focus_hint.is_empty(),
        "the message pane has focus, so there is nothing to explain"
    );
    press(&mut model, Key::Char('h'));
    assert!(
        !bar(&model).focus_hint.is_empty(),
        "eligible: the folder pane has focus"
    );
    assert!(
        !status_row(&model, 120).contains("focus: folders"),
        "and not shown, because at this width that pane is drawn"
    );
    assert!(
        status_row(&model, 50).contains("focus: folders"),
        "shown below the breakpoint, where it is not"
    );
}

#[test]
fn the_pick_overlay_does_not_claim_the_folder_pane_has_focus() {
    let mut model = loaded();
    press(&mut model, Key::Char('h'));
    press(&mut model, Key::Char('c'));
    assert_eq!(model.mode(), Mode::Pick);
    assert!(
        bar(&model).focus_hint.is_empty(),
        "a picker is not the folder pane, and the hint names a `<tab>` that \\
         would do nothing here"
    );
}
