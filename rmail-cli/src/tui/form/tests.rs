//! The form pane's own mechanics: what a field holds, what the line it applies
//! says, and the two things `<esc>` can mean.
//!
//! The *policy* claims — a bare `:ai budget set` reading before it writes, a
//! bang skipping the form — are in `tui::commands::ai_policy::tests`, next to
//! the verb that makes them. What is here is the pane: it has no idea which RPC
//! it is editing, and every claim below holds for whatever verb task 101's
//! settings screen puts behind it.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use rmail_core::command::Resolution;

use super::*;

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn invocation(line: &str) -> Invocation {
    match command::parse(line) {
        Ok(Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
    }
}

/// A two-field form over `line`, which is a real declared verb — so the line it
/// rebuilds is one the parser accepts.
fn pane(line: &str) -> FormPane {
    FormPane::new(
        invocation(line),
        "ai budget set",
        vec![
            Field::new("daily-hard-usd", "daily hard $", "block at", String::new()),
            Field::new(
                "monthly-hard-usd",
                "monthly hard $",
                "block at",
                String::new(),
            ),
        ],
        5,
    )
}

/// A filled form, which is the only kind that can be applied.
fn filled(line: &str, values: &[(&str, &str)]) -> FormPane {
    let mut pane = pane(line);
    let values: Vec<(String, String)> = values
        .iter()
        .map(|(flag, value)| ((*flag).to_owned(), (*value).to_owned()))
        .collect();
    assert!(pane.fill(5, &values));
    pane
}

// ---------------------------------------------------------------------------
// rows
// ---------------------------------------------------------------------------

#[test]
fn the_apply_row_is_a_row() {
    // `Menu` has one gesture for "use what is highlighted", so the commit has to
    // *be* a row rather than a key of its own — otherwise a form would need a
    // second vocabulary nobody else here uses.
    let mut pane = pane("ai budget set");
    assert_eq!(pane.rows(), 3);
    assert!(!pane.on_apply());
    pane.cursor = 1;
    assert!(!pane.on_apply());
    pane.cursor = 2;
    assert!(pane.on_apply());
    assert!(pane.field().is_none());
}

#[test]
fn editing_never_opens_on_the_apply_row() {
    // There is no value there, and putting the keyboard into insert mode over it
    // would leave somebody typing into a button.
    let mut pane = pane("ai budget set");
    pane.cursor = 2;
    pane.edit();
    assert!(pane.editing.is_none());
}

// ---------------------------------------------------------------------------
// filling in
// ---------------------------------------------------------------------------

#[test]
fn a_form_is_un_appliable_until_the_read_lands() {
    let mut pane = pane("ai budget set");
    let why = pane.blocked().expect("an unfilled form is blocked");
    assert!(why.contains("nothing to replace"), "{why}");
    assert!(pane.fill(5, &[]));
    assert!(pane.blocked().is_none());
}

#[test]
fn a_failed_read_says_why_and_stays_blocked() {
    // A form that could not see what it is replacing must not replace it — and
    // the reason has to be the thing on screen, not a generic "not yet".
    let mut pane = pane("ai budget set");
    assert!(pane.fail(5, "the daemon is not running".to_owned()));
    let why = pane.blocked().expect("still blocked");
    assert!(why.contains("the daemon is not running"), "{why}");
    assert!(why.contains("nothing was changed"), "{why}");
}

#[test]
fn an_answer_to_another_request_is_dropped() {
    let mut pane = pane("ai budget set");
    assert!(!pane.fill(4, &[("daily-hard-usd".to_owned(), "9".to_owned())]));
    assert!(!pane.fail(6, "boom".to_owned()));
    assert!(!pane.ready);
    assert!(pane.error.is_none());
    assert!(pane.fields.iter().all(|field| field.value.is_empty()));
}

#[test]
fn the_typed_line_wins_over_what_the_daemon_reported() {
    // What "flags pre-fill the form" means: the daemon supplies the caps in
    // force so applying replaces them with themselves, and the line then
    // overwrites the one field it named. The other way round, a typed flag would
    // be silently discarded by the answer it was waiting for.
    let pane = filled(
        "ai budget set --daily-hard-usd=5",
        &[("daily-hard-usd", "9"), ("monthly-hard-usd", "50")],
    );
    assert_eq!(pane.fields[0].value, "5");
    assert_eq!(pane.fields[1].value, "50");
}

#[test]
fn a_reported_value_this_build_has_no_field_for_is_ignored() {
    // A newer daemon reporting a cap this client does not draw is not a bug in
    // this client, and refusing the whole answer over it would make one new
    // proto field break the form.
    let pane = filled(
        "ai budget set",
        &[("weekly-hard-usd", "5"), ("daily-hard-usd", "1")],
    );
    assert_eq!(pane.fields[0].value, "1");
    assert!(pane.ready);
}

#[test]
fn a_field_is_bounded_and_sanitised() {
    // Bounded: a value is a number, and a key held down against it must not grow
    // a `String` for as long as it is leaned on. Sanitised: what fills it comes
    // off the wire.
    //
    // `MAX_VALUE` exactly, not `MAX_VALUE + 1`: `overlays::truncate_chars` puts
    // its ellipsis *past* the limit it is given, which is right for a table cell
    // and would be a field one character over the length `push` refuses at — so
    // it could never be edited back into bounds.
    let pane = filled(
        "ai budget set",
        &[("daily-hard-usd", &"9".repeat(MAX_VALUE + 40))],
    );
    assert_eq!(pane.fields[0].value.chars().count(), MAX_VALUE);
    assert!(pane.fields[0].value.ends_with('…'), "the cut is marked");
    // A value that fits is not marked, and keeps every character.
    let exact = "9".repeat(MAX_VALUE);
    let pane = filled("ai budget set", &[("daily-hard-usd", &exact)]);
    assert_eq!(pane.fields[0].value, exact);

    let pane = filled("ai budget set", &[("daily-hard-usd", "1\n2\t3")]);
    assert!(
        !pane.fields[0].value.contains('\n'),
        "{:?}",
        pane.fields[0].value
    );
}

#[test]
fn typing_stops_at_the_cap() {
    let mut pane = pane("ai budget set");
    pane.edit();
    for _ in 0..MAX_VALUE + 10 {
        pane.push('9');
    }
    assert_eq!(pane.fields[0].value.chars().count(), MAX_VALUE);
    pane.backspace();
    assert_eq!(pane.fields[0].value.chars().count(), MAX_VALUE - 1);
}

// ---------------------------------------------------------------------------
// the open field, and the cursor
// ---------------------------------------------------------------------------

#[test]
fn an_edit_belongs_to_the_field_it_opened_on() {
    // The cursor and the open field are separate facts: a build with
    // `cursor.down` bound in the `insert` layer could move one without the
    // other, and an edit keyed off the cursor would then put one field's text
    // back into another.
    let mut pane = filled("ai budget set", &[("daily-hard-usd", "1")]);
    pane.edit();
    pane.push('2');
    pane.cursor = 1;
    pane.push('3');
    assert_eq!(pane.fields[0].value, "123", "typing follows the open field");
    assert_eq!(pane.fields[1].value, "");
    assert!(pane.cancel_edit());
    assert_eq!(pane.fields[0].value, "1", "and so does putting it back");
    assert_eq!(pane.fields[1].value, "");
}

#[test]
fn cancelling_says_whether_there_was_anything_to_cancel() {
    // What makes `<esc>` close the innermost thing: the caller can tell "the
    // edit was cancelled" from "there was nothing open, so close the form".
    let mut pane = pane("ai budget set");
    assert!(!pane.cancel_edit());
    pane.edit();
    assert!(pane.cancel_edit());
    assert!(!pane.cancel_edit());
}

#[test]
fn committing_keeps_what_was_typed() {
    let mut pane = pane("ai budget set");
    pane.edit();
    pane.push('7');
    pane.commit();
    assert!(pane.editing.is_none());
    assert_eq!(pane.fields[0].value, "7");
    // And a later cancel cannot undo a committed edit — there is nothing left
    // holding the old value.
    assert!(!pane.cancel_edit());
    assert_eq!(pane.fields[0].value, "7");
}

// ---------------------------------------------------------------------------
// the line it applies
// ---------------------------------------------------------------------------

#[test]
fn the_line_carries_the_flags_the_fields_do_not_own() {
    // `--account` and `--bulk` choose *which* budget is being replaced. A form
    // that dropped them would replace the global one however it was opened —
    // a wrong answer that looks exactly like the right one.
    let pane = filled(
        "ai budget set --account=3 --bulk",
        &[("daily-hard-usd", "5")],
    );
    assert_eq!(
        pane.line(),
        "ai budget set --account=3 --bulk --daily-hard-usd=5!"
    );
}

#[test]
fn an_empty_field_contributes_no_flag() {
    // Which is how "no cap" is expressed: `SetBudget` replaces the whole scope,
    // so a cap the line omits is a cap cleared. Clearing one is a thing somebody
    // does on purpose, and emptying the field is how they do it.
    let pane = filled("ai budget set", &[("monthly-hard-usd", "50")]);
    assert_eq!(pane.line(), "ai budget set --monthly-hard-usd=50!");
}

#[test]
fn the_line_is_banged() {
    // The form *is* the deliberate act — it opened, it was read, it was applied
    // — and without the bang applying would re-enter the same dispatch and open
    // a second form over the first.
    let pane = filled("ai budget set", &[]);
    assert!(pane.line().ends_with('!'));
    assert!(pane.apply().expect("parses").bang);
}

#[test]
fn applying_goes_through_the_parser() {
    // So applying a form and typing the same line are one code path from here
    // on: one dispatcher, one place a bang is honoured, and a form that cannot
    // do something a typed line could not.
    let pane = filled(
        "ai budget set --bulk",
        &[("daily-hard-usd", "5"), ("monthly-hard-usd", "50")],
    );
    let applied = pane.apply().expect("the rebuilt line parses");
    assert_eq!(applied.verb, vec!["ai", "budget", "set"]);
    let mut flags: Vec<(String, Option<String>)> = applied
        .flags
        .iter()
        .map(|flag| (flag.name.clone(), flag.value.clone()))
        .collect();
    flags.sort();
    assert_eq!(
        flags,
        vec![
            ("bulk".to_owned(), None),
            ("daily-hard-usd".to_owned(), Some("5".to_owned())),
            ("monthly-hard-usd".to_owned(), Some("50".to_owned())),
        ]
    );
}

#[test]
fn a_value_with_a_space_or_a_quote_in_it_survives_the_round_trip() {
    // A field takes whatever was typed into it. Pasted into a line unquoted, a
    // space would split one value into two tokens and a quote would end the line
    // early — so a form could produce a line that parsed to something nobody
    // asked for.
    let mut pane = pane("ai budget set");
    pane.edit();
    for c in "a \"b".chars() {
        pane.push(c);
    }
    pane.commit();
    let applied = pane.apply().expect("the rebuilt line parses");
    assert_eq!(
        applied
            .flags
            .iter()
            .find(|flag| flag.name == "daily-hard-usd")
            .and_then(|flag| flag.value.clone()),
        Some("a \"b".to_owned())
    );
}

#[test]
fn a_positional_the_line_carried_is_carried_through() {
    // No form's verb takes one today. It is carried anyway because the
    // alternative is a form that silently drops an argument the moment task
    // 101 puts one behind a field set.
    let mut pane = FormPane::new(
        invocation("ai provider set local"),
        "ai provider set",
        vec![Field::new(
            "account",
            "account",
            "which scope",
            String::new(),
        )],
        5,
    );
    assert!(pane.fill(5, &[("account".to_owned(), "3".to_owned())]));
    assert_eq!(pane.line(), "ai provider set local --account=3!");
    let applied = pane.apply().expect("parses");
    assert_eq!(applied.positionals, vec!["local".to_owned()]);
}
