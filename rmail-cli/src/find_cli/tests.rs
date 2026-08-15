use rmail_proto::v1::{FindResult, ItemKind as ProtoItemKind};

use super::{kind_name, sanitize, to_json, ScopeArg};

fn result() -> FindResult {
    FindResult {
        item_id: 41,
        kind: ProtoItemKind::Message as i32,
        ref_id: 4471,
        score: 138.5,
        primary_text: "Invoice #338 — Acme".to_owned(),
        secondary: "billing@acme.com".to_owned(),
        positions: vec![0, 8, 9],
        account_id: 1,
        mailbox_id: 2,
    }
}

/// The `--json` contract, asserted by key set and by spelling. A proto field
/// rename must not silently reshape it — see the module docs.
#[test]
fn the_json_shape_is_the_documented_one() {
    let line = serde_json::to_string(&to_json(&result())).expect("serializes");
    let value: serde_json::Value = serde_json::from_str(&line).expect("valid json");
    let object = value.as_object().expect("an object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "account_id",
            "item_id",
            "kind",
            "mailbox_id",
            "positions",
            "ref_id",
            "score",
            "secondary",
            "text",
        ]
    );
    assert_eq!(object["kind"], "message");
    assert_eq!(object["ref_id"], 4471);
    assert_eq!(object["text"], "Invoice #338 — Acme");
}

/// The `--json` positions are the daemon's char offsets, passed through
/// untouched — a consumer indexes `text.chars()` with them.
#[test]
fn json_positions_index_chars_not_bytes() {
    let mut hit = result();
    hit.primary_text = "Café résumé".to_owned();
    hit.positions = vec![0, 5];
    let json = to_json(&hit);
    let chars: Vec<char> = json.text.chars().collect();
    for position in &json.positions {
        assert!(
            (*position as usize) < chars.len(),
            "{position} out of range"
        );
    }
    assert_eq!(chars[json.positions[0] as usize], 'C');
    assert_eq!(chars[json.positions[1] as usize], 'r');
    // Char 5 is the `r` of `résumé`; byte 5 is the space before it, and byte
    // 4 is *inside* the two-byte `é`. Reading these offsets as bytes would
    // therefore both point at the wrong character and, one index earlier,
    // land mid-character — the exact mistake `search_cli`'s own snippet test
    // pins for highlight ranges.
    assert!(!json.text.is_char_boundary(4));
    assert_ne!(
        json.text.as_bytes()[json.positions[1] as usize],
        b'r',
        "byte 5 is not the character char-index 5 names"
    );
}

/// The kind spelling is a CLI contract, not the generated enum's name.
#[test]
fn kind_names_are_the_stable_lowercase_ones() {
    assert_eq!(kind_name(ProtoItemKind::Message as i32), "message");
    assert_eq!(kind_name(ProtoItemKind::Mailbox as i32), "mailbox");
    assert_eq!(kind_name(ProtoItemKind::Contact as i32), "contact");
    assert_eq!(kind_name(ProtoItemKind::SavedSearch as i32), "saved_search");
    assert_eq!(kind_name(ProtoItemKind::Tag as i32), "tag");
    assert_eq!(kind_name(ProtoItemKind::Command as i32), "command");
    // A kind from a newer daemon prints something rather than panicking.
    assert_eq!(kind_name(9_999), "unknown");
}

/// A subject line is attacker-controlled, and the table prints it close to
/// verbatim. `ESC` must never reach the terminal.
#[test]
fn the_table_strips_control_characters() {
    let hostile = "Invoice \u{1b}[31mRED\u{1b}[0m \u{7} done";
    let clean = sanitize(hostile);
    assert!(!clean.contains('\u{1b}'), "an ESC survived: {clean:?}");
    assert!(!clean.contains('\u{7}'));
    assert!(clean.contains("Invoice"));
    assert!(clean.contains("RED"));
}

/// Newlines become spaces rather than vanishing, so a folded subject still
/// reads as one row instead of welding two words together.
#[test]
fn whitespace_controls_become_spaces() {
    assert_eq!(sanitize("one\ttwo\nthree\rfour"), "one two three four");
}

/// Multi-byte text must come through the sanitizer intact — it filters by
/// character, not by byte.
#[test]
fn sanitizing_leaves_non_ascii_text_alone() {
    assert_eq!(sanitize("Café 会議 résumé"), "Café 会議 résumé");
}

#[test]
fn every_scope_arg_maps_to_a_wire_scope() {
    use rmail_proto::v1::FinderScope;
    for (arg, wire) in [
        (ScopeArg::All, FinderScope::All),
        (ScopeArg::Messages, FinderScope::Messages),
        (ScopeArg::Mailboxes, FinderScope::Mailboxes),
        (ScopeArg::Contacts, FinderScope::Contacts),
        (ScopeArg::Searches, FinderScope::SavedSearches),
        (ScopeArg::Tags, FinderScope::Tags),
        (ScopeArg::Commands, FinderScope::Commands),
    ] {
        assert_eq!(arg.into_proto(), wire, "{arg:?}");
        assert_ne!(arg.into_proto(), FinderScope::Unspecified);
    }
}
