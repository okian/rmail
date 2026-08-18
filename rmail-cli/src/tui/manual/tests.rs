//! The manual's renderer, its generated pages, and the reconciliation suite
//! that keeps the two honest against the registries they read.
//!
//! The reconciliation tests are the point of this module. Everything the
//! manual says about a key, a command or a capability is resolved against the
//! live [`Keymap`], `rmail_core::command`'s verb registry or
//! [`Capability::ALL`] at render time — so a rebind, a renamed verb or a new
//! RPC either shows up in the manual or fails a test here by name. A manual
//! checked only by reading it is a manual that is wrong by the second release.
//!
//! `panic!`/`unwrap` in a branch that cannot happen reads better here than
//! the `unreachable!` dance, and this module is test-only — the same
//! exemption `tui::model::tests` and `tui::view::tests` take.
#![allow(clippy::panic)]

use super::*;
use crate::keymap::{Chord, Keymap};

fn keymap() -> Keymap {
    Keymap::defaults()
}

/// What `render_blocks` puts in front of a fenced-code line, and therefore
/// what identifies a generated table's row on a rendered page.
const CODE_GUTTER: &str = "  │ ";

/// One page's rendered lines, styling dropped.
fn page_lines(page: &Page, keymap: &Keymap) -> Vec<String> {
    doc(&Location::Page(page.anchor.to_owned()), keymap)
        .lines
        .iter()
        .map(DocLine::text)
        .collect()
}

/// Render one snippet of markdown as if it were a page, with no footer.
fn render(markdown: &str, keymap: &Keymap) -> Vec<DocLine> {
    render_blocks(&parse_blocks(markdown), keymap)
}

/// The runs of `markdown` rendered as a single flat sequence — for asserting
/// about inks without caring which line something landed on.
fn flat(markdown: &str, keymap: &Keymap) -> Vec<Run> {
    render(markdown, keymap)
        .into_iter()
        .flat_map(|line| line.runs)
        .collect()
}

fn text_of(lines: &[DocLine]) -> String {
    lines
        .iter()
        .map(DocLine::text)
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// reconciliation — the tests this module exists for
// ---------------------------------------------------------------------------

/// Every rendered line of every page, styling dropped — what the "is this
/// documented anywhere" checks scan.
fn all_lines(keymap: &Keymap) -> Vec<String> {
    PAGES
        .iter()
        .flat_map(|page| page_lines(page, keymap))
        .collect()
}

/// Whether any line names `token` as a whole word.
///
/// Substring matching is not good enough for any of these checks and quietly
/// weakens them below what their names claim: `help` is a substring of
/// `helpgrep`, `manual` of `manual.back`, `search` of `search.explain`. Either
/// of the shorter ids could vanish from the key reference entirely and a
/// `contains` would still be satisfied by its longer sibling — so the check
/// that is supposed to catch a table being truncated would not.
fn names(lines: &[String], token: &str) -> bool {
    lines.iter().any(|line| {
        line.split(|c: char| c.is_whitespace() || c == ',')
            .any(|word| word == token)
    })
}

/// Every line inside a fenced block of `markdown`, blanks dropped — the rows
/// of a generated table.
fn fenced_rows(markdown: &str) -> Vec<&str> {
    let mut rows = Vec::new();
    let mut fenced = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced && !line.trim().is_empty() {
            rows.push(line);
        }
    }
    rows
}

/// Task 103's first reconciliation criterion — "every registry verb has a page
/// anchor" — discharged as *set equality* against the rendered page rather
/// than as "its spelling appears somewhere".
///
/// The weaker form could not fail, and not only because the generator
/// enumerates the registry: for the verb `search`, the row `search explain …`
/// contains the word `search`, so deleting `search`'s own row left every
/// substring-or-whole-word check satisfied by its sibling. Comparing the whole
/// row set catches a truncated table, a duplicated row and a drifted
/// description, and cannot be satisfied by a different verb's row.
#[test]
fn every_registry_verb_has_its_own_row_on_the_command_index() {
    let keymap = keymap();
    let index = page("commands").expect("the command index is in the registry");
    // The rendered row is the generated one behind `render_blocks`' code
    // gutter, so the gutter is what identifies a table row on the page.
    let printed: BTreeSet<String> = page_lines(index, &keymap)
        .iter()
        .filter_map(|line| line.strip_prefix(CODE_GUTTER))
        .map(|row| row.trim_end().to_owned())
        .collect();
    let expected: BTreeSet<String> = command::children_of(&[])
        .into_iter()
        .map(|verb| {
            format!("{:<34} {}", signature(verb), verb.describe())
                .trim_end()
                .to_owned()
        })
        .collect();
    assert_eq!(
        printed, expected,
        "the command index and the verb registry disagree"
    );
}

/// The scan above is only as strong as its generator, so this is the test that
/// proves the generator is what carries it: strip the command index out of the
/// page set and coverage collapses. Without this, a `generate_commands` that
/// printed nothing at all would fail
/// `every_registry_verb_is_documented_somewhere` for a reason nobody reading
/// it could locate.
#[test]
fn it_is_the_generated_index_that_makes_verb_coverage_hold() {
    let keymap = keymap();
    let authored: Vec<String> = PAGES
        .iter()
        .filter(|page| matches!(page.body, Body::Authored(_)))
        .flat_map(|page| page_lines(page, &keymap))
        .collect();
    let uncovered = command::children_of(&[])
        .into_iter()
        .filter(|verb| !authored.iter().any(|line| line.contains(&verb.canonical())))
        .count();
    assert!(
        uncovered > 0,
        "every verb is named by authored prose, so the generated index is no \
         longer what the coverage check depends on — update the check's \
         docstring and task 104's acceptance rather than deleting this test"
    );
}

/// Task 103's second and third criteria at once: a `[[link]]` that resolves to
/// no page and a `{{…}}` that resolves to nothing both render as
/// [`Ink::Broken`], so one assertion covers both — and it covers exactly the
/// markers that *render*, leaving the ones shown verbatim inside a fence
/// (`PAGES`' own `manual` page documents the syntax) alone, which is what a
/// scan of the raw markdown could not do.
#[test]
fn no_link_or_expansion_in_any_page_is_broken() {
    let keymap = keymap();
    for page in PAGES {
        let rendered = doc(&Location::Page(page.anchor.to_owned()), &keymap);
        for (idx, line) in rendered.lines.iter().enumerate() {
            let broken: Vec<&str> = line
                .runs
                .iter()
                .filter(|run| run.ink == Ink::Broken)
                .map(|run| run.text.as_str())
                .collect();
            assert!(
                broken.is_empty(),
                "{}:{} does not resolve: {broken:?}",
                page.anchor,
                idx + 1
            );
        }
    }
}

/// Task 103's fourth criterion.
#[test]
fn every_action_id_is_documented_somewhere() {
    let lines = all_lines(&keymap());
    for action in Action::ALL {
        assert!(
            names(&lines, action.id()),
            "no manual page names the action `{}` — the generated key \
             reference lists every bound one and every unbound one, so this \
             means neither list saw it",
            action.id()
        );
    }
}

/// Task 104's own acceptance criterion, enforced here because this is where
/// the enforcement belongs — the generated capability page makes it true for
/// every page set, authored or not.
#[test]
fn every_capability_with_a_tui_surface_is_documented() {
    let lines = all_lines(&keymap());
    for capability in Capability::ALL {
        if capability.actions().is_empty() {
            continue;
        }
        assert!(
            names(&lines, capability.name()),
            "{} has a TUI action but no manual page names it",
            capability.name()
        );
    }
}

#[test]
fn every_page_but_the_front_one_is_linked_from_another_page() {
    let keymap = keymap();
    let mut linked: BTreeSet<&str> = BTreeSet::new();
    for page in PAGES {
        for line in doc(&Location::Page(page.anchor.to_owned()), &keymap).lines {
            for run in line.runs {
                if let Some(target) = run.link {
                    linked.insert(target);
                }
            }
        }
    }
    for page in PAGES {
        assert!(
            page.anchor == START || linked.contains(page.anchor),
            "{} is a page nothing links to — it can only be reached by \
             editing the source",
            page.anchor
        );
    }
}

#[test]
fn anchors_and_titles_are_unique_and_non_empty() {
    let anchors: BTreeSet<&str> = PAGES.iter().map(|page| page.anchor).collect();
    assert_eq!(anchors.len(), PAGES.len(), "two pages share an anchor");
    let titles: BTreeSet<&str> = PAGES.iter().map(|page| page.title).collect();
    assert_eq!(titles.len(), PAGES.len(), "two pages share a title");
    for page in PAGES {
        assert!(
            !page.anchor.is_empty() && !page.title.is_empty(),
            "{page:?}"
        );
        assert!(
            !page.anchor.contains(' '),
            "{} is an anchor with a space in it; `[[…]]` trims but does not \
             split, so it would never resolve",
            page.anchor
        );
    }
}

#[test]
fn every_authored_page_closes_its_fences() {
    for page in PAGES {
        let Body::Authored(source) = page.body else {
            continue;
        };
        let fences = source
            .lines()
            .filter(|line| line.trim_start().starts_with("```"))
            .count();
        assert_eq!(
            fences % 2,
            0,
            "{} has an unclosed fence, so everything after it renders \
             verbatim",
            page.anchor
        );
    }
}

#[test]
fn every_page_renders_at_least_something() {
    let keymap = keymap();
    for page in PAGES {
        let rendered = doc(&Location::Page(page.anchor.to_owned()), &keymap);
        assert!(!rendered.lines.is_empty(), "{} rendered empty", page.anchor);
        assert_eq!(rendered.title, page.title);
    }
}

#[test]
fn no_rendered_prose_line_runs_past_the_wrap_column() {
    let keymap = keymap();
    for page in PAGES {
        for (idx, line) in doc(&Location::Page(page.anchor.to_owned()), &keymap)
            .lines
            .iter()
            .enumerate()
        {
            // Fenced code is verbatim by definition, gutter included, and the
            // generated pages' tables are deliberately wider than prose.
            let is_code = line.runs.iter().any(|run| run.ink == Ink::Code);
            if is_code {
                continue;
            }
            let width = line.text().chars().count();
            assert!(
                width <= WRAP,
                "{}:{} is {width} columns wide: {:?}",
                page.anchor,
                idx + 1,
                line.text()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// the renderer's six constructs
// ---------------------------------------------------------------------------

#[test]
fn headings_bullets_and_code_each_render_as_themselves() {
    let lines = render(
        "# One\n\n## Two\n\nprose\n\n- a bullet\n  - nested\n\n```\nverbatim  spacing\n```\n",
        &keymap(),
    );
    let flat = text_of(&lines);
    assert!(flat.contains("One"), "{flat}");
    assert!(flat.contains("Two"));
    assert!(flat.contains("prose"));
    assert!(flat.contains("• a bullet"));
    assert!(
        flat.contains("  • nested"),
        "a nested bullet is indented: {flat}"
    );
    assert!(
        flat.contains("│ verbatim  spacing"),
        "fenced code keeps its own spacing and gets a gutter: {flat}"
    );

    let inks: Vec<Ink> = lines
        .iter()
        .flat_map(|line| line.runs.iter().map(|run| run.ink))
        .collect();
    assert!(inks.contains(&Ink::Heading));
    assert!(inks.contains(&Ink::Code));
    assert!(inks.contains(&Ink::Accent), "the bullet marker");
}

#[test]
fn four_hashes_is_prose_not_a_heading() {
    // The grammar stops at three levels on purpose. A `####` line has to
    // render as *something*, and rendering it as text is the only answer that
    // does not silently drop what the author wrote.
    let lines = render("#### deeper\n", &keymap());
    assert!(text_of(&lines).contains("#### deeper"));
    assert!(lines
        .iter()
        .flat_map(|line| &line.runs)
        .all(|run| run.ink != Ink::Heading));
}

#[test]
fn a_hash_with_no_space_after_it_is_prose() {
    let lines = render("#nothashtag\n", &keymap());
    assert!(text_of(&lines).contains("#nothashtag"));
}

#[test]
fn consecutive_prose_lines_reflow_into_one_paragraph() {
    // An authored page hard-wraps its own source; keeping those breaks would
    // wrap the paragraph twice and leave it ragged at whatever column the
    // author's editor used.
    let lines = render("one\ntwo\nthree\n", &keymap());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].text(), "one two three");
}

#[test]
fn a_blank_line_separates_paragraphs() {
    let lines = render("one\n\ntwo\n", &keymap());
    assert_eq!(text_of(&lines), "one\n\ntwo");
}

#[test]
fn a_long_paragraph_wraps_at_the_wrap_column() {
    let source = "word ".repeat(60);
    let lines = render(&source, &keymap());
    assert!(lines.len() > 1, "it wrapped");
    for line in &lines {
        assert!(line.text().chars().count() <= WRAP, "{:?}", line.text());
    }
    // Nothing is lost or duplicated by wrapping.
    let round_trip: Vec<String> = lines
        .iter()
        .flat_map(|line| {
            line.text()
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(round_trip.len(), 60);
}

#[test]
fn a_word_longer_than_the_line_takes_a_line_of_its_own_rather_than_being_split() {
    let long = "x".repeat(WRAP + 20);
    let lines = render(&format!("short {long} short\n"), &keymap());
    assert!(
        lines.iter().any(|line| line.text().trim() == long),
        "the long word survived whole: {:?}",
        text_of(&lines)
    );
}

#[test]
fn a_wrapped_bullet_hangs_under_its_own_text() {
    let lines = render(&format!("- {}\n", "word ".repeat(40)), &keymap());
    assert!(lines.len() > 1);
    assert!(lines[0].text().starts_with("• "));
    assert!(
        lines[1].text().starts_with("  ") && !lines[1].text().starts_with("  •"),
        "the continuation is indented to the marker's width, not re-bulleted: \
         {:?}",
        lines[1].text()
    );
}

#[test]
fn trailing_blank_lines_are_trimmed() {
    let lines = render("prose\n\n\n\n", &keymap());
    assert_eq!(lines.len(), 1);
}

// ---------------------------------------------------------------------------
// links
// ---------------------------------------------------------------------------

#[test]
fn a_link_renders_the_target_pages_own_title_and_carries_its_anchor() {
    let lines = render("see [[keys]] for more\n", &keymap());
    let key_page = page("keys").expect("the keys page is in the registry");
    assert!(
        lines[0].text().contains(key_page.title),
        "the label is the target's title, not its anchor: {:?}",
        lines[0].text()
    );
    assert_eq!(lines[0].link(), Some("keys"));
}

#[test]
fn a_link_to_a_page_that_does_not_exist_is_broken_not_silent() {
    let runs = flat("see [[nowhere]]\n", &keymap());
    let broken = runs
        .iter()
        .find(|run| run.ink == Ink::Broken)
        .expect("a dangling link renders as broken");
    assert!(broken.text.contains("nowhere"));
    assert_eq!(broken.link, None, "a broken link is not followable");
}

#[test]
fn a_line_with_several_links_follows_the_first() {
    let lines = render("[[keys]] and [[modes]]\n", &keymap());
    assert_eq!(lines[0].link(), Some("keys"));
}

#[test]
fn an_unclosed_marker_is_literal_text() {
    // A page discussing the syntax outside a fence must not be a parse error.
    let lines = render("an open [[bracket and nothing else\n", &keymap());
    assert!(
        lines[0].text().contains("[[bracket"),
        "{:?}",
        lines[0].text()
    );
}

// ---------------------------------------------------------------------------
// expansions
// ---------------------------------------------------------------------------

#[test]
fn a_keys_expansion_names_the_chords_actually_in_force() {
    let mut keymap = keymap();
    let runs = flat("archive is {{keys:message.archive}}\n", &keymap);
    let chord = runs
        .iter()
        .find(|run| run.ink == Ink::Chord)
        .expect("the chord run");
    assert_eq!(chord.text, "a");

    // The whole reason this is an expansion rather than prose: rebind it and
    // the page says so, with no page edited and no restart.
    keymap
        .bind(
            crate::keymap::Mode::Normal,
            Chord::parse("Z").unwrap(),
            Action::Archive,
        )
        .unwrap();
    let runs = flat("archive is {{keys:message.archive}}\n", &keymap);
    let chord = runs
        .iter()
        .find(|run| run.ink == Ink::Chord)
        .expect("the chord run");
    assert_eq!(chord.text, "Z / a", "both ways to press it, in layer order");
}

#[test]
fn a_keys_expansion_for_an_action_nothing_binds_says_unbound() {
    let mut keymap = keymap();
    keymap.unbind(crate::keymap::Mode::Normal, &Chord::parse("a").unwrap());
    let runs = flat("{{keys:message.archive}}\n", &keymap);
    assert!(
        runs.iter().any(|run| run.text == "unbound"),
        "an unbound action is a fact about the keymap, not a broken \
         expansion: {runs:?}"
    );
    assert!(
        runs.iter().all(|run| run.ink != Ink::Broken),
        "and it is not reported as broken"
    );
}

#[test]
fn a_keys_expansion_naming_no_such_action_is_broken() {
    let runs = flat("{{keys:message.telepathy}}\n", &keymap());
    assert!(runs.iter().any(|run| run.ink == Ink::Broken), "{runs:?}");
}

#[test]
fn a_cmd_expansion_renders_the_verb_and_checks_it_against_the_registry() {
    let runs = flat("run {{cmd:message archive}}\n", &keymap());
    assert!(
        runs.iter()
            .any(|run| run.text == ":message archive" && run.ink == Ink::Accent),
        "{runs:?}"
    );

    // Dots and spaces are the same separator, exactly as the parser treats
    // them — a page may write either.
    let dotted = flat("run {{cmd:message.archive}}\n", &keymap());
    assert!(dotted.iter().any(|run| run.text == ":message archive"));

    let unknown = flat("{{cmd:message telepathy}}\n", &keymap());
    assert!(unknown.iter().any(|run| run.ink == Ink::Broken));
}

#[test]
fn a_capability_expansion_renders_the_rpc_and_checks_the_variant_name() {
    let runs = flat("behind it: {{capability:MailSetFlags}}\n", &keymap());
    assert!(
        runs.iter().any(|run| run.text == "MailService.SetFlags"),
        "{runs:?}"
    );
    let unknown = flat("{{capability:MailTelepathy}}\n", &keymap());
    assert!(unknown.iter().any(|run| run.ink == Ink::Broken));
}

#[test]
fn an_expansion_of_an_unknown_kind_is_broken_rather_than_dropped() {
    for source in ["{{colour:red}}\n", "{{nocolon}}\n"] {
        let runs = flat(source, &keymap());
        assert!(
            runs.iter().any(|run| run.ink == Ink::Broken),
            "{source:?} rendered without complaining: {runs:?}"
        );
    }
}

#[test]
fn a_heading_keeps_its_ink_over_an_expansion_but_not_over_a_broken_one() {
    let keymap = keymap();
    let heading = flat("# archive with {{keys:message.archive}}\n", &keymap);
    assert!(
        heading.iter().all(|run| run.ink == Ink::Heading),
        "a heading is a heading throughout: {heading:?}"
    );
    let broken = flat("# {{keys:no.such.action}}\n", &keymap);
    assert!(
        broken.iter().any(|run| run.ink == Ink::Broken),
        "except where something did not resolve, which a heading must not \
         hide: {broken:?}"
    );
}

#[test]
fn every_marker_renders_as_one_wrapping_unit() {
    // All three marker forms can contain a space — a link's label is the
    // target's multi-word title, a chord list is joined with " / ", a verb
    // path has segments. Wrapping any of them as prose would put half at the
    // end of one line and half at the start of the next.
    let keymap = keymap();
    for source in [
        "{{cmd:message archive}}",
        "[[keys]]",
        "{{keys:cursor.down}}",
    ] {
        let runs = flat(&format!("{source}\n"), &keymap);
        let atoms: Vec<&Run> = runs.iter().filter(|run| run.atomic).collect();
        assert_eq!(
            atoms.len(),
            1,
            "{source} did not render as one unit: {runs:?}"
        );
        assert!(
            atoms[0].text.contains(' '),
            "{source} is meant to be the multi-word case: {:?}",
            atoms[0].text
        );
    }
}

#[test]
fn a_marker_that_does_not_fit_moves_to_the_next_line_whole() {
    let keymap = keymap();
    let filler = "word ".repeat(15);
    let lines = render(
        &format!("{filler}{{{{cmd:message archive}}}} end\n"),
        &keymap,
    );
    assert!(lines.len() > 1, "it wrapped: {:?}", text_of(&lines));
    assert!(
        lines
            .iter()
            .any(|line| line.text().contains(":message archive")),
        "the verb survived the line break whole: {:?}",
        text_of(&lines)
    );
}

#[test]
fn a_wrapped_headings_spaces_belong_to_the_heading() {
    // A gap inked as prose would render as a hole in the heading under any
    // theme whose heading token paints a background.
    let lines = render(&format!("# {}\n", "word ".repeat(40)), &keymap());
    assert!(lines.len() > 1);
    for line in &lines {
        for run in &line.runs {
            assert_eq!(run.ink, Ink::Heading, "{:?}", line.text());
        }
    }
}

#[test]
fn markers_inside_a_fence_are_verbatim() {
    let lines = render(
        "```\n{{keys:message.archive}} and [[keys]]\n```\n",
        &keymap(),
    );
    let flat = text_of(&lines);
    assert!(
        flat.contains("{{keys:message.archive}}") && flat.contains("[[keys]]"),
        "a page documenting the syntax has to be able to show it: {flat}"
    );
    assert_eq!(
        lines.iter().filter_map(DocLine::link).count(),
        0,
        "and nothing in a fence is followable"
    );
}

// ---------------------------------------------------------------------------
// the capability footer
// ---------------------------------------------------------------------------

#[test]
fn a_page_naming_a_command_gets_a_footer_derived_from_it() {
    let blocks = parse_blocks("run {{cmd:message delete}} carefully\n");
    let footer = text_of(&footer(&blocks));
    assert!(footer.contains("What this page reaches"), "{footer}");
    assert!(
        footer.contains("MailService.Delete"),
        "the footer names the RPC behind the verb the page mentioned: {footer}"
    );
    assert!(
        footer.contains("mutates"),
        "and whether calling it changes anything: {footer}"
    );
}

#[test]
fn a_page_naming_no_command_gets_no_footer() {
    assert!(footer(&parse_blocks("just prose\n")).is_empty());
}

#[test]
fn a_command_shown_inside_a_fence_is_not_a_command_the_page_reaches() {
    // Otherwise the manual page explaining `{{cmd:…}}` would claim every RPC
    // it used as an example.
    let blocks = parse_blocks("```\n{{cmd:message delete}}\n```\n");
    assert!(footer(&blocks).is_empty());
    assert!(cited_verbs(&blocks).is_empty());
}

#[test]
fn a_ui_only_verb_contributes_no_footer_row() {
    // `helpgrep` reaches no RPC at all, which is the manual's own case.
    let blocks = parse_blocks("see {{cmd:helpgrep}}\n");
    assert_eq!(cited_verbs(&blocks).len(), 1);
    assert!(footer(&blocks).is_empty());
}

// ---------------------------------------------------------------------------
// generated pages
// ---------------------------------------------------------------------------

#[test]
fn the_key_reference_lists_every_layer_and_reflects_a_rebind() {
    let mut keymap = keymap();
    let before = generate_keys(&keymap);
    for mode in layers() {
        if keymap.layer(mode).count() > 0 {
            assert!(
                before.contains(&format!("## {}", mode.id())),
                "{} has bindings but no section",
                mode.id()
            );
        }
    }
    assert!(
        before.contains("cursor.down"),
        "an action id is what the row is keyed by"
    );

    keymap
        .bind(
            crate::keymap::Mode::Normal,
            Chord::parse("Z").unwrap(),
            Action::Archive,
        )
        .unwrap();
    let after = generate_keys(&keymap);
    assert!(after.contains("Z "), "the new chord shows up: {after}");
    assert_ne!(before, after);
}

#[test]
fn the_key_reference_prints_one_row_per_binding_in_every_layer() {
    // The count, not just the presence: `every_action_id_is_documented_somewhere`
    // is satisfied by the union of "bound" and "unbound", which is total by
    // construction — so a generator that printed only the first row of a layer
    // would pass it. This is the check that catches that, per layer, against
    // the keymap's own count.
    let keymap = keymap();
    let page = generate_keys(&keymap);
    for mode in layers() {
        let expected = keymap.layer(mode).count();
        if expected == 0 {
            continue;
        }
        let heading = format!("## {}\n", mode.id());
        let (_, after) = page
            .split_once(&heading)
            .unwrap_or_else(|| panic!("{} has {expected} bindings but no section", mode.id()));
        // Stop at the next heading, so a layer with no rows of its own cannot
        // be credited with the following layer's.
        let section = after.split("\n## ").next().unwrap_or(after);
        let rows = fenced_rows(section).len();
        assert_eq!(
            rows,
            expected,
            "{} prints {rows} rows for {expected} bindings",
            mode.id()
        );
    }
    assert!(page.contains("Actions no key runs"));
}

#[test]
fn the_key_reference_lists_a_genuinely_unbound_action_under_its_own_heading() {
    let mut keymap = keymap();
    keymap.unbind(crate::keymap::Mode::Normal, &Chord::parse("a").unwrap());
    let page = generate_keys(&keymap);
    let (_, unbound) = page
        .split_once("Actions no key runs")
        .expect("the unbound section");
    assert!(
        unbound.contains("message.archive"),
        "an action whose only chord was removed moves to the unbound list: \
         {unbound}"
    );
}

#[test]
fn the_command_index_shows_a_verbs_arguments_in_its_signature() {
    // Coverage is `every_registry_verb_has_its_own_row_on_the_command_index`'s
    // job; this is about how one row *reads*.
    let page = generate_commands();
    assert!(
        page.contains("helpgrep [pattern]"),
        "an optional positional is shown in brackets: {page}"
    );
    assert!(
        page.contains("manual grep [pattern]"),
        "including on the namespaced spelling of the same action: {page}"
    );
}

#[test]
fn the_mode_diagram_lists_every_mode_and_its_chain() {
    let page = generate_modes();
    for mode in layers() {
        assert!(page.contains(mode.id()), "{} is missing", mode.id());
        let chain: Vec<&str> = mode.chain().iter().map(|layer| layer.id()).collect();
        assert!(
            page.contains(&chain.join(" → ")),
            "{}'s chain is missing: {page}",
            mode.id()
        );
    }
    assert!(
        page.contains("global"),
        "including the layer no config file may name"
    );
}

#[test]
fn the_capability_page_separates_what_the_tui_reaches_from_everything() {
    let page = generate_capabilities();
    let (reachable, _) = page
        .split_once("## Every capability")
        .expect("both sections exist");
    for capability in Capability::ALL {
        assert!(
            page.contains(capability.name()),
            "{} is missing entirely",
            capability.name()
        );
        if capability.actions().is_empty() {
            continue;
        }
        let rpc = format!("{}.{}", short_service(*capability), capability.method());
        assert!(
            reachable.contains(&rpc),
            "{rpc} has a TUI action but is not in the reachable section"
        );
    }
}

// ---------------------------------------------------------------------------
// grep
// ---------------------------------------------------------------------------

#[test]
fn grep_finds_a_phrase_and_says_which_page_and_line_it_is_on() {
    let keymap = keymap();
    let hits = grep("Actions no key runs", &keymap);
    assert!(!hits.is_empty(), "the generated keys page has that heading");
    let hit = &hits[0];
    assert_eq!(hit.anchor, "keys");
    assert_eq!(hit.title, "Key reference");
    assert!(hit.line >= 1, "line numbers are 1-based");
    let lines = page_lines(page("keys").unwrap(), &keymap);
    assert!(
        lines[hit.line - 1].contains("Actions no key runs"),
        "the line number addresses the rendered page: {:?}",
        lines[hit.line - 1]
    );
}

#[test]
fn grep_is_case_insensitive() {
    let keymap = keymap();
    assert_eq!(
        grep("KEY REFERENCE", &keymap).len(),
        grep("key reference", &keymap).len()
    );
}

#[test]
fn an_empty_or_blank_grep_pattern_finds_nothing() {
    let keymap = keymap();
    for pattern in ["", "   ", "\t"] {
        assert!(
            grep(pattern, &keymap).is_empty(),
            "{pattern:?} matched something — searching for nothing must not \
             mean matching everything"
        );
    }
}

#[test]
fn grep_stops_at_its_cap() {
    let keymap = keymap();
    // "e" is on most lines of the manual, which is the point: the assertion
    // is exact rather than an inequality, so it fails if the early return is
    // ever removed *or* if it fires early.
    let total: usize = PAGES
        .iter()
        .map(|page| {
            page_lines(page, &keymap)
                .iter()
                .filter(|line| line.to_lowercase().contains('e'))
                .count()
        })
        .sum();
    assert_eq!(grep("e", &keymap).len(), total.min(MAX_HITS));
}

#[test]
fn the_grep_page_lists_its_hits_as_followable_rows() {
    let keymap = keymap();
    let rendered = doc(&Location::Grep("Command index".to_owned()), &keymap);
    assert!(rendered.title.contains("Command index"));
    let followable: Vec<&'static str> = rendered.lines.iter().filter_map(DocLine::link).collect();
    assert!(
        followable.contains(&"commands"),
        "a hit on the command index is a row that opens it: {followable:?}"
    );
}

#[test]
fn the_grep_page_says_so_when_nothing_matches() {
    let rendered = doc(&Location::Grep("zzzznope".to_owned()), &keymap());
    assert!(text_of(&rendered.lines).contains("No page mentions it"));
}

#[test]
fn a_grep_hit_containing_markdown_is_not_reparsed_as_markdown() {
    // The hit list is built from *rendered* lines. Running them back through
    // the block parser would turn a page's own bullet into this page's
    // bullet, and a `[[…]]` it merely displayed into a link nobody wrote.
    let keymap = keymap();
    let rendered = doc(&Location::Grep("[[keys]]".to_owned()), &keymap);
    let flat = text_of(&rendered.lines);
    assert!(
        flat.contains("[[keys]]"),
        "the manual page shows that marker verbatim inside a fence, so grep \
         should find it: {flat}"
    );
    // The only followable rows are the hit headers, which link to the page
    // the hit is on — never to whatever the hit text happened to spell.
    for line in &rendered.lines {
        if let Some(target) = line.link() {
            assert!(
                page(target).is_some(),
                "{target} is not a page, so it came from re-parsing hit text"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// in-page search
// ---------------------------------------------------------------------------

#[test]
fn matching_lines_reports_every_line_containing_the_pattern() {
    let keymap = keymap();
    let rendered = doc(&Location::Page("keys".to_owned()), &keymap);
    let lines = matching_lines(&rendered, "cursor.down");
    assert!(!lines.is_empty());
    for idx in lines {
        assert!(rendered.lines[idx].text().contains("cursor.down"));
    }
}

#[test]
fn matching_lines_finds_a_pattern_that_straddles_two_runs() {
    // `[[keys]]` renders as a link run followed by a plain one, so "reference
    // for" spans the boundary. Locating has to work on the whole line even
    // though highlighting can only paint within one run.
    let keymap = keymap();
    let rendered = Doc {
        title: String::new(),
        lines: render("[[keys]] for more\n", &keymap),
    };
    assert_eq!(matching_lines(&rendered, "reference for"), vec![0]);
}

#[test]
fn highlighting_splits_runs_without_changing_the_line_count() {
    let keymap = keymap();
    let mut rendered = doc(&Location::Page("keys".to_owned()), &keymap);
    let before = rendered.lines.len();
    let text_before = text_of(&rendered.lines);
    highlight(&mut rendered, "cursor");
    assert_eq!(
        rendered.lines.len(),
        before,
        "the cursor, the scroll offset and every hit's line number all depend \
         on this"
    );
    assert_eq!(
        text_of(&rendered.lines),
        text_before,
        "and not one character moved"
    );
    assert!(rendered
        .lines
        .iter()
        .any(|line| line.runs.iter().any(|run| run.ink == Ink::Match)));
}

#[test]
fn highlighting_is_case_insensitive_and_marks_only_the_match() {
    let mut rendered = Doc {
        title: String::new(),
        lines: vec![DocLine::from_runs(vec![Run::new(
            "Archive the ARCHIVE archive",
            Ink::Body,
        )])],
    };
    highlight(&mut rendered, "archive");
    let matched: Vec<&str> = rendered.lines[0]
        .runs
        .iter()
        .filter(|run| run.ink == Ink::Match)
        .map(|run| run.text.as_str())
        .collect();
    assert_eq!(matched, vec!["Archive", "ARCHIVE", "archive"]);
    assert_eq!(rendered.lines[0].text(), "Archive the ARCHIVE archive");
}

#[test]
fn highlighting_survives_text_whose_case_folding_changes_its_length() {
    // `İ` (U+0130) folds to two code points and `K` (U+212A) folds from three
    // bytes to one, so a byte offset taken from a `to_lowercase()` copy would
    // land inside a character here. `split_on` folds forwards for exactly
    // this; the assertion is that nothing panics and no text is lost.
    for text in ["İstanbul kebab", "Kelvin K and k", "café CAFÉ"] {
        for pattern in ["k", "a", "é", "istanbul"] {
            let mut rendered = Doc {
                title: String::new(),
                lines: vec![DocLine::from_runs(vec![Run::new(text, Ink::Body)])],
            };
            highlight(&mut rendered, pattern);
            assert_eq!(
                rendered.lines[0].text(),
                text,
                "highlighting {pattern:?} in {text:?} changed the text"
            );
        }
    }
}

#[test]
fn highlighting_an_empty_pattern_changes_nothing() {
    let keymap = keymap();
    let before = doc(&Location::Page("keys".to_owned()), &keymap);
    let mut after = before.clone();
    highlight(&mut after, "  ");
    assert_eq!(before, after);
}

// ---------------------------------------------------------------------------
// totality
// ---------------------------------------------------------------------------

#[test]
fn a_location_naming_no_such_page_renders_a_way_back_rather_than_failing() {
    let rendered = doc(&Location::Page("nowhere".to_owned()), &keymap());
    assert!(text_of(&rendered.lines).contains("nowhere"));
    assert_eq!(
        rendered.lines.iter().filter_map(DocLine::link).next(),
        Some(START),
        "and the one link on it goes home"
    );
}

#[test]
fn a_location_labels_itself_for_the_status_line() {
    assert_eq!(Location::start().label(), "Start here");
    assert_eq!(
        Location::Grep("invoice".to_owned()).label(),
        "helpgrep \"invoice\""
    );
    assert!(Location::Page("nowhere".to_owned())
        .label()
        .contains("no such page"));
}

#[test]
fn the_whole_manual_renders_with_every_built_in_theme_of_bindings() {
    // The manual is the one screen that must work when everything else is
    // broken, so "renders at all" is checked against a keymap with nothing in
    // it as well as the default one — a user who unbound their way into a
    // corner still has to be able to read how to get out.
    let mut stripped = Keymap::defaults();
    for mode in layers() {
        let chords: Vec<Chord> = stripped
            .layer(mode)
            .map(|(chord, _)| chord.clone())
            .collect();
        for chord in chords {
            stripped.unbind(mode, &chord);
        }
    }
    for keymap in [Keymap::defaults(), stripped] {
        for page in PAGES {
            let rendered = doc(&Location::Page(page.anchor.to_owned()), &keymap);
            assert!(!rendered.lines.is_empty(), "{} rendered empty", page.anchor);
        }
    }
}

#[test]
fn there_is_no_horizontal_rule_construct() {
    // "Nothing else" taken literally: a `---` line is prose, and the only
    // separator the manual draws is the one a generated footer puts above
    // itself. A construct no page needs is one nobody would notice breaking.
    let lines = render("above\n\n---\n\nbelow\n", &keymap());
    assert_eq!(text_of(&lines), "above\n\n---\n\nbelow");
}

#[test]
fn an_indented_line_after_a_bullet_continues_that_bullet() {
    // The form both shipped pages are written in. Before this parsed, a
    // bullet's tail became a *separate flush-left paragraph* sitting between
    // two bullets — wrapped at column 0 rather than hanging under its own
    // text, which is the opposite of what the hanging indent is for.
    let lines = render("- first line\n  second line\n- next bullet\n", &keymap());
    assert_eq!(
        text_of(&lines),
        "• first line second line\n• next bullet",
        "the continuation joined its own bullet"
    );
}

#[test]
fn a_long_two_line_bullet_hangs_rather_than_starting_a_paragraph() {
    let lines = render(
        &format!("- {}\n  {}\n", "word ".repeat(20), "more ".repeat(20)),
        &keymap(),
    );
    assert!(lines.len() > 1);
    assert!(lines[0].text().starts_with("• "));
    for line in &lines[1..] {
        assert!(
            line.text().starts_with("  ") && !line.text().starts_with("  •"),
            "every continuation hangs under the bullet's text: {:?}",
            line.text()
        );
    }
}

#[test]
fn an_unindented_line_after_a_bullet_is_still_a_new_paragraph() {
    // Markdown's *lazy* continuation would swallow this; requiring the indent
    // is what keeps a paragraph deliberately following a list a paragraph.
    let lines = render("- a bullet\nprose after the list\n", &keymap());
    assert_eq!(text_of(&lines), "• a bullet\nprose after the list");
}

#[test]
fn an_indented_line_after_a_paragraph_that_followed_a_bullet_is_not_glued_back_on() {
    let lines = render("- a bullet\nprose\n  indented\n", &keymap());
    assert_eq!(
        text_of(&lines),
        "• a bullet\nprose indented",
        "the paragraph interrupted the bullet, so the indent joins the \
         paragraph rather than reaching back past it"
    );
}

#[test]
fn a_nested_bullets_continuation_hangs_at_its_own_depth() {
    let lines = render("  - nested\n    continued\n", &keymap());
    assert_eq!(text_of(&lines), "  • nested continued");
}

/// Where a source line sits relative to the list before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListState {
    /// Not in a list item.
    Outside,
    /// Inside one — a bullet line, or an indented continuation of it.
    Inside,
    /// Inside one, but a blank line has intervened.
    AfterBlank,
}

#[test]
fn no_authored_page_writes_a_list_shape_the_renderer_does_not_support() {
    // Checked on the *source*, because both shapes below are source shapes
    // whose renders are indistinguishable from legitimate output — a
    // rendered-pair scan skipped one of them entirely (it looks at a
    // hang-indented line, not a bullet line) and could not see the other at
    // all. Neither is in the two shipped pages; task 104 is about to write
    // forty more, and this is what tells its author which is which.
    for page in PAGES {
        let Body::Authored(source) = page.body else {
            continue;
        };
        let mut fenced = false;
        let mut state = ListState::Outside;
        for (idx, line) in source.lines().enumerate() {
            let at = format!("{}:{}", page.anchor, idx + 1);
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                state = ListState::Outside;
                continue;
            }
            if fenced {
                continue;
            }
            let blank = line.trim().is_empty();
            let bullet = line.trim_start().starts_with("- ");
            let indented = line.starts_with(char::is_whitespace);
            let heading = !indented && line.starts_with('#');

            assert!(
                !(state == ListState::Inside && !blank && !bullet && !indented && !heading),
                "{at}: an unindented line continuing a bullet renders as a \
                 flush-left paragraph inside the list — indent it by two to \
                 continue the bullet, or leave a blank line to start a real \
                 paragraph"
            );
            assert!(
                !(state == ListState::AfterBlank && indented && !blank && !bullet),
                "{at}: a second paragraph inside a list item renders \
                 flush-left — this renderer's list item is one paragraph, so \
                 split it into two bullets or drop the blank line"
            );

            state = if bullet {
                ListState::Inside
            } else if blank && state != ListState::Outside {
                ListState::AfterBlank
            } else if indented && state == ListState::Inside {
                ListState::Inside
            } else {
                ListState::Outside
            };
        }
    }
}

#[test]
fn punctuation_glued_to_a_marker_in_the_source_stays_glued() {
    // A page names a key mid-sentence as `({{keys:open}})`, and wrapping by
    // whitespace-free *runs* rather than by source whitespace rendered that as
    // `( <enter> )` — three units with the wrapper's own spaces between them.
    let keymap = keymap();
    for (source, expected) in [
        ("press ({{keys:open}}) to open", "press (<enter>) to open"),
        ("see [[keys]], then stop", "see Key reference, then stop"),
        ("run {{cmd:message archive}}.", "run :message archive."),
        (
            "both {{keys:open}}/{{keys:back}} work",
            "both <enter>/q work",
        ),
    ] {
        let lines = render(&format!("{source}\n"), &keymap);
        assert_eq!(text_of(&lines), expected, "source: {source:?}");
    }
}

#[test]
fn a_glued_group_breaks_the_line_outside_its_punctuation() {
    let keymap = keymap();
    let lines = render(
        &format!("{}({{{{cmd:message archive}}}}) end\n", "word ".repeat(15)),
        &keymap,
    );
    assert!(lines.len() > 1, "it wrapped: {:?}", text_of(&lines));
    assert!(
        lines
            .iter()
            .any(|line| line.text().contains("(:message archive)")),
        "the parenthesis moved with what it wrapped: {:?}",
        text_of(&lines)
    );
}

#[test]
fn no_authored_prose_renders_a_space_inside_a_parenthesis() {
    // The shape the defect above produced, checked against the real pages —
    // and only the authored ones. Scanning the generated pages too would make
    // this test about `Capability::summary()` and `Action::describe()`, so a
    // capability row in another crate could fail a test about the manual's
    // line wrapper. Fenced rows are verbatim anyway, which is what those
    // pages are made of.
    let keymap = keymap();
    for page in PAGES {
        if !matches!(page.body, Body::Authored(_)) {
            continue;
        }
        for line in page_lines(page, &keymap) {
            if line.starts_with(CODE_GUTTER) {
                continue;
            }
            assert!(
                !line.contains("( ") && !line.contains(" )"),
                "{}: {line:?}",
                page.anchor
            );
        }
    }
}
