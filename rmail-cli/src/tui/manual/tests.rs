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

use std::collections::BTreeMap;

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

/// The scan above is only as strong as its generator, so this is the test
/// that proves the generator is what carries it: strip the command index out
/// of the page set and coverage collapses.
///
/// Task 104's acceptance predicted this would "fail by name once authored
/// prose covers everything", and it does not — forty pages later it is still
/// green, for a reason worth recording rather than deleting: authored prose
/// names a verb with `{{cmd:…}}`, which renders `:message archive`, and names
/// an action with `{{keys:…}}`, which renders a *chord*. So the ~22 verbs a
/// page only ever names by key — `cursor up`, `visual toggle`, `manual back`,
/// `input submit` — appear in no authored line at all, and the generated
/// index is still doing the work. What task 104 actually added is
/// [`every_action_and_verb_has_exactly_one_documenting_page`], which is a
/// stronger statement in a different direction; it does not subsume this one,
/// so this one stays.
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
         longer what the coverage check depends on — update this docstring \
         rather than deleting the test"
    );
}

/// Task 104's `Action`/[`Verb`] → anchor mapping, discharged as a bijection
/// rather than as coverage.
///
/// The mapping task 102's `K`-on-a-row needs, and the one thing "every
/// registry verb has a page anchor" never actually was: 103 discharged that
/// clause as "its spelling appears on some page", which answers *whether* a
/// verb is documented and not *where*. This answers where, in both
/// directions — every action and verb has exactly one home, and every
/// declared home is a real action or verb.
#[test]
fn every_action_and_verb_has_exactly_one_documenting_page() {
    let mut claimed: BTreeMap<Vec<String>, Vec<&str>> = BTreeMap::new();
    for page in PAGES {
        for id in page.documents {
            claimed.entry(split_path(id)).or_default().push(page.anchor);
        }
    }
    for (id, pages) in &claimed {
        assert_eq!(
            pages.len(),
            1,
            "{} is claimed by {pages:?} — an id has one home, or `home_of` \
             answers whichever page happens to be listed first",
            id.join(" ")
        );
        let path: Vec<&str> = id.iter().map(String::as_str).collect();
        assert!(
            Action::from_id(&id.join(".")).is_some() || command::verb_at(&path).is_some(),
            "{} is documented by {pages:?} and is neither an action nor a \
             verb",
            id.join(" ")
        );
    }
    for action in Action::ALL {
        assert!(
            home_of(action.id()).is_some(),
            "no page declares itself the home of `{}` — add its id to that \
             page's `documents`",
            action.id()
        );
    }
    for verb in command::children_of(&[]) {
        assert!(
            home_of(&verb.canonical()).is_some(),
            "no page declares itself the home of `:{}`",
            verb.canonical()
        );
    }
}

/// A declaration that the page does not back up is worse than none: it sends
/// a reader to a page that never mentions what they asked about.
///
/// "Backed up" means the page's own source cites the id in a `{{keys:…}}` or
/// `{{cmd:…}}` marker outside a fence — the same two forms the reconciliation
/// above already resolves against the live registries, so a page cannot
/// satisfy this with prose that merely spells the id out.
#[test]
fn a_pages_declared_documentation_is_backed_by_its_own_prose() {
    for page in PAGES {
        if page.documents.is_empty() {
            continue;
        }
        let Body::Authored(source) = page.body else {
            panic!(
                "{} is generated and declares {:?} — a generated page is a \
                 derivation of a registry, not a home for one of its rows",
                page.anchor, page.documents
            );
        };
        let cited = cited_ids(source);
        for id in page.documents {
            assert!(
                cited.contains(&split_path(id)),
                "{} claims to document `{id}` and never names it",
                page.anchor
            );
        }
    }
}

/// Every id a page names with `{{keys:…}}` or `{{cmd:…}}` outside a fence,
/// split the way the verb registry splits a path.
///
/// Reads the parsed blocks rather than the raw source for the same reason
/// [`footer`] does: a marker shown *inside* a fence is documentation of the
/// syntax, not a claim about a key — `PAGES`' own `manual` page writes
/// several.
fn cited_ids(source: &str) -> BTreeSet<Vec<String>> {
    let mut cited = BTreeSet::new();
    for block in parse_blocks(source) {
        let text = match block {
            Block::Heading { text, .. } | Block::Bullet { text, .. } | Block::Para(text) => text,
            Block::Code(_) | Block::Blank => continue,
        };
        for marker in ["{{keys:", "{{cmd:"] {
            let mut rest = text.as_str();
            while let Some(at) = rest.find(marker) {
                let after = at + marker.len();
                let Some(end) = rest.get(after..).and_then(|tail| tail.find("}}")) else {
                    break;
                };
                let path = rest.get(after..after + end).unwrap_or_default().trim();
                cited.insert(split_path(path));
                rest = rest.get(after + end + 2..).unwrap_or_default();
            }
        }
    }
    cited
}

/// `model::open_manual_at` resolves a page anchor first and a documented id
/// second, so an anchor that is *also* a documented id must resolve the same
/// way both ways round or the precedence silently decides which page a reader
/// gets.
///
/// Today `manual` is the only such string and the two agree, which is why the
/// behavioural test in `model::tests` cannot tell the two orderings apart —
/// this is the check that can, and it fails the day task 105's new vocabulary
/// introduces an action id spelled like some other page's anchor.
#[test]
fn a_page_anchor_that_is_also_a_documented_id_resolves_to_its_own_page() {
    for page in PAGES {
        if let Some(home) = home_of(page.anchor) {
            assert_eq!(
                home.anchor, page.anchor,
                "`{}` is a page anchor and is documented by `{}` — \
                 `open_manual_at` prefers the anchor, so a reader asking for \
                 the action lands somewhere else",
                page.anchor, home.anchor
            );
        }
    }
}

/// [`home_of`] answers the same page whichever separator the caller uses,
/// because task 102's `K` has an [`Action::id`] and task 89's `:` has a verb
/// path, and they are the same thing spelled two ways.
#[test]
fn home_of_resolves_an_action_id_and_a_verb_path_to_the_same_page() {
    for action in Action::ALL {
        let by_id = home_of(action.id()).map(|page| page.anchor);
        let by_path = home_of(&action.id().replace('.', " ")).map(|page| page.anchor);
        assert_eq!(by_id, by_path, "{}", action.id());
    }
    assert_eq!(
        home_of("message.archive").map(|page| page.anchor),
        Some("archive")
    );
    assert_eq!(home_of("helpgrep").map(|page| page.anchor), Some("manual"));
    assert_eq!(home_of("no.such.thing"), None);
    assert_eq!(home_of(""), None);
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

/// Task 104's own acceptance criterion, scanning **authored** pages only.
///
/// Task 103 wrote this over every page, which could not fail: the generated
/// capability page enumerates [`Capability::ALL`] by construction, so forty
/// pages mentioning no capability at all would still have passed. Narrowing
/// the scan is what makes it a statement about prose.
///
/// A capability counts as named when an authored page renders its
/// `Service.Method` — which happens either from a `{{capability:…}}` marker
/// or from the page's derived "reaches" footer, and the footer is itself
/// derived from the `{{cmd:…}}` verbs the page names. Both are therefore
/// claims the page actually made about a command it discusses, rather than a
/// row in a table.
#[test]
fn every_capability_with_a_tui_surface_is_documented() {
    let keymap = keymap();
    let authored: Vec<String> = PAGES
        .iter()
        .filter(|page| matches!(page.body, Body::Authored(_)))
        .flat_map(|page| page_lines(page, &keymap))
        .collect();
    for capability in Capability::ALL {
        if capability.actions().is_empty() {
            continue;
        }
        let rendered = format!("{}.{}", short_service(*capability), capability.method());
        assert!(
            names(&authored, &rendered),
            "{} has a TUI action and no authored page names it — cite a \
             command that reaches it, or name it with a capability marker",
            capability.name()
        );
    }
}

/// The generated page is still what documents every capability no key or
/// command reaches, and this is what says so: strip the authored pages out
/// and the coverage holds anyway — which is exactly the property the narrowed
/// scan above stops depending on.
///
/// Asserted over [`Capability::ALL`] rather than over the TUI-surfaced
/// subset, because the generated page's claim is the wider one and a check
/// scoped to the narrow set would say less than the page does.
#[test]
fn the_generated_page_covers_every_capability() {
    let keymap = keymap();
    let generated: Vec<String> = PAGES
        .iter()
        .filter(|page| matches!(page.body, Body::Generated(_)))
        .flat_map(|page| page_lines(page, &keymap))
        .collect();
    for capability in Capability::ALL {
        assert!(
            names(&generated, capability.name()),
            "{} is in no generated page",
            capability.name()
        );
    }
}

/// A page whose heading and whose registry title disagree shows two
/// different names for itself in the same frame — the pane title comes from
/// [`Page::title`] and the first line of the page comes from its own source.
///
/// [`START`] is the declared exception: its heading is the product's name,
/// which is what a front page opens with, and "Start here" is what a link to
/// it should read as. One page where those are legitimately different is a
/// reason for an exception, not for no check.
#[test]
fn every_authored_page_heads_itself_with_its_own_title() {
    for page in PAGES {
        let Body::Authored(source) = page.body else {
            continue;
        };
        if page.anchor == START {
            continue;
        }
        let heading = source
            .lines()
            .find_map(|line| line.strip_prefix("# "))
            .unwrap_or_else(|| panic!("{} opens with no heading", page.anchor));
        assert_eq!(
            heading.trim(),
            page.title,
            "{}'s heading and its registry title disagree",
            page.anchor
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
    // all. Neither is in any shipped page, and this is what kept them out of
    // task 104's forty — both were written by hand while drafting them and
    // both were caught here rather than by reading the render.
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

// ---------------------------------------------------------------------------
// authored shell transcripts, reconciled against `clap`
// ---------------------------------------------------------------------------

/// Every `mail …` line an authored page shows in a fence is one this binary
/// would actually accept.
///
/// The gap this closes is the one that made task 104's first draft wrong in
/// seventeen places: `{{cmd:…}}` and `{{capability:…}}` are checked against
/// the registries, but a fenced transcript is prose the suite never read — so
/// `mail rule backtest`, a verb that does not exist, and `mail webhook add
/// --url …`, a flag that is a positional, both rendered happily. A manual
/// whose worked examples do not run is worse than one with no worked
/// examples, because the reader blames themselves.
///
/// Checked against `Cli::command()` — the same tree that parses an argv, so a
/// renamed flag fails here the moment it compiles — rather than by running
/// anything: the values in these lines are placeholders, and a check that
/// needed real ones could only ever have been a second, hand-written list.
/// Verb path, long and short options, option-vs-positional, and the presence
/// of every required argument are all read off that tree. Values are not, so
/// a placeholder `<id>` standing where an integer goes is out of scope here
/// and is what the `--help` output remains authoritative for.
#[test]
fn every_shell_command_an_authored_page_shows_is_one_this_binary_accepts() {
    use clap::CommandFactory as _;

    let mut root = crate::Cli::command();
    // Propagates `#[arg(global = true)]` down every subcommand, which is
    // where `--socket`, `--token` and `--format` live. Without it every
    // documented line carrying one would be reported as an unknown flag.
    root.build();
    // Every failure, not the first: a page's transcripts tend to be wrong
    // together, and fixing them one panic at a time is how the seventeenth
    // gets missed.
    let mut wrong: Vec<String> = Vec::new();
    let mut checked = 0_usize;
    for page in PAGES {
        let Body::Authored(source) = page.body else {
            continue;
        };
        for (line_no, line) in shell_lines(source) {
            checked += 1;
            if let Err(why) = accepts(&root, &tokenize(&line)) {
                wrong.push(format!("{}:{line_no}: `{line}` — {why}", page.anchor));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    // A tokenizer that quietly matched nothing would make the assertion above
    // vacuous, and the shape of these pages is what decides how many lines it
    // sees — so this is a floor, not a fixed count.
    assert!(
        checked >= 40,
        "only {checked} shell lines were checked — `shell_lines` stopped \
         seeing the transcripts it is supposed to reconcile"
    );
}

/// The `mail …` command lines inside `source`'s fences, with their 1-based
/// source line numbers.
///
/// Continuation backslashes are joined first, so a wrapped invocation is
/// checked whole. A leading `VAR=value` is dropped: `RUST_LOG=debug mail
/// daemon start` is a shell line about `mail daemon start`.
fn shell_lines(source: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut fenced = false;
    let mut pending: Option<(usize, String)> = None;
    for (idx, raw) in source.lines().enumerate() {
        if raw.trim_start().starts_with("```") {
            fenced = !fenced;
            pending = None;
            continue;
        }
        if !fenced {
            continue;
        }
        let line = raw.trim();
        let (at, mut joined) = match pending.take() {
            Some((at, mut prefix)) => {
                prefix.push(' ');
                prefix.push_str(line);
                (at, prefix)
            }
            None => (idx + 1, line.to_owned()),
        };
        if let Some(head) = joined.strip_suffix('\\') {
            pending = Some((at, head.trim_end().to_owned()));
            continue;
        }
        joined.truncate(annotation(&joined));
        let words: Vec<&str> = joined.split_whitespace().collect();
        let first = words
            .iter()
            .position(|word| !(word.contains('=') && !word.starts_with('-')));
        if let Some(first) = first {
            if words.get(first) == Some(&"mail") {
                out.push((at, words[first..].join(" ")));
            }
        }
    }
    out
}

/// Where a fenced line stops being a command and starts being an annotation:
/// a shell comment, or the aligned gloss these pages put in the right-hand
/// column of a listing.
///
/// The gloss is two-or-more spaces outside quotes, which is the convention
/// the pages already use and the only one that can be told apart from an
/// argument. Inside quotes it means nothing — `mail search "a  b"` is one
/// argument with two spaces in it, and truncating there would report a
/// working command line as broken.
fn annotation(line: &str) -> usize {
    let mut quote: Option<char> = None;
    let mut run = 0_usize;
    for (at, ch) in line.char_indices() {
        match quote {
            Some(open) => {
                if ch == open {
                    quote = None;
                }
                run = 0;
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                run = 0;
            }
            None if ch == '#' && run > 0 => return at.saturating_sub(run),
            None if ch == ' ' => {
                run += 1;
                if run == 2 {
                    return at - 1;
                }
            }
            None => run = 0,
        }
    }
    line.len()
}

/// Split a command line on whitespace, keeping quoted runs together.
fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in line.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                started = true;
            }
            None if ch.is_whitespace() => {
                if started || !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => current.push(ch),
        }
    }
    if started || !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Whether `tokens` — `mail`, then a command line — is one `root` accepts.
fn accepts(root: &clap::Command, tokens: &[String]) -> Result<(), String> {
    let mut node = root;
    let mut at = 1;
    while let Some(sub) = tokens.get(at).and_then(|word| node.find_subcommand(word)) {
        node = sub;
        at += 1;
    }
    let path = tokens[..at].join(" ");
    if node.is_subcommand_required_set() {
        return Err(format!(
            "`{path}` is a grouping and needs one of: {}",
            node.get_subcommands()
                .map(clap::Command::get_name)
                .filter(|name| *name != "help")
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let mut given: BTreeSet<&str> = BTreeSet::new();
    let mut positionals = 0_usize;
    while at < tokens.len() {
        let token = tokens[at].as_str();
        at += 1;
        let arg = if let Some(long) = token.strip_prefix("--") {
            let (name, inline) = long
                .split_once('=')
                .map_or((long, None), |(n, v)| (n, Some(v)));
            let arg =
                find_long(node, name).ok_or_else(|| format!("`{path}` has no `--{name}` flag"))?;
            if inline.is_none() && arg.get_action().takes_values() {
                at += 1;
            }
            arg
        } else if token.len() > 1 && token.starts_with('-') {
            let short = token.chars().nth(1).unwrap_or('-');
            let arg = find_short(node, short)
                .ok_or_else(|| format!("`{path}` has no `-{short}` flag"))?;
            if token.len() == 2 && arg.get_action().takes_values() {
                at += 1;
            }
            arg
        } else {
            positionals += 1;
            continue;
        };
        given.insert(arg.get_id().as_str());
    }

    let slots: usize = node
        .get_arguments()
        .filter(|arg| arg.is_positional())
        .map(|arg| arg.get_num_args().map_or(1, |range| range.max_values()))
        .sum();
    if positionals > slots {
        return Err(format!(
            "`{path}` takes {slots} positional argument(s) and {positionals} were given"
        ));
    }
    let mut required_positionals = 0_usize;
    for arg in node.get_arguments().filter(|arg| arg.is_required_set()) {
        if arg.is_positional() {
            required_positionals += 1;
            continue;
        }
        if !given.contains(arg.get_id().as_str()) {
            return Err(format!(
                "`{path}` requires `--{}`",
                arg.get_long().unwrap_or_else(|| arg.get_id().as_str())
            ));
        }
    }
    if positionals < required_positionals {
        return Err(format!(
            "`{path}` requires {required_positionals} positional argument(s), not {positionals}"
        ));
    }
    Ok(())
}

fn find_long<'a>(node: &'a clap::Command, name: &str) -> Option<&'a clap::Arg> {
    node.get_arguments().find(|arg| {
        arg.get_long() == Some(name)
            || arg
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&name))
    })
}

fn find_short(node: &clap::Command, short: char) -> Option<&clap::Arg> {
    node.get_arguments()
        .find(|arg| arg.get_short() == Some(short))
}

/// Every `:` line an authored page shows in a fence parses, and carries only
/// a range the TUI actually honours.
///
/// The sibling of
/// [`every_shell_command_an_authored_page_shows_is_one_this_binary_accepts`],
/// and the same gap: task 89's pages gained `:'<,'>message archive` in
/// fences, and `shell_lines` only reads lines beginning `mail`. Both happen
/// to be right today; nothing kept them so. A `%` or a count in one of these
/// would be a page documenting a range the model refuses.
#[test]
fn every_colon_line_an_authored_page_shows_parses_and_uses_an_honoured_range() {
    let mut checked = 0_usize;
    for page in PAGES {
        let Body::Authored(source) = page.body else {
            continue;
        };
        for (line_no, line) in fenced_lines(source, ':') {
            checked += 1;
            let at = format!("{}:{line_no}", page.anchor);
            let typed = line.trim_start_matches(':');
            match command::parse(typed) {
                Ok(command::Resolution::Invocation(invocation)) => {
                    assert!(
                        !matches!(
                            invocation.range,
                            Some(command::Range::All | command::Range::Count(_))
                        ),
                        "{at}: `{line}` shows a range this TUI refuses — see \
                         `model::unsupported_range`"
                    );
                }
                other => panic!("{at}: `{line}` does not resolve to a verb: {other:?}"),
            }
        }
    }
    assert!(
        checked >= 3,
        "only {checked} colon lines were seen — the scan stopped finding them"
    );
}

/// The lines inside `source`'s fences that begin with `lead`, with their
/// 1-based source line numbers.
fn fenced_lines(source: &str, lead: char) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut fenced = false;
    for (idx, raw) in source.lines().enumerate() {
        if raw.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        // Column zero, deliberately, and not `trim()`: a `:` is also the
        // finder's mailbox sigil, and `search-vs-finder` shows a table of
        // those. An indented fenced line is a table row; a flush-left one
        // beginning with `:` is a command line. That is the convention this
        // scan is, and the reason the sigil table is indented.
        if fenced && raw.starts_with(lead) {
            let line = raw.trim_end();
            out.push((idx + 1, line[..annotation(line)].trim_end().to_owned()));
        }
    }
    out
}
