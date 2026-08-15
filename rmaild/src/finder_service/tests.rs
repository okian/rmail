//! Unit tests for the pieces of `FinderService` that an end-to-end test
//! cannot force: the generation slot's ordering, and the two wire mappings
//! whose numbering is deliberately *not* derived from each other.

use rmail_core::finder::ItemKind;
use rmail_proto::v1::{FinderScope as ProtoScope, ItemKind as ProtoItemKind};
use tokio_util::sync::CancellationToken;

use super::{
    decode_action, decode_scope, require_message_kind, to_proto_kind, Generation, ItemAction,
    MAX_BATCH_REFS,
};
use rmail_core::finder::Scope;

/// The whole cancellation contract in one assertion: a new stream cancels
/// the previous one and leaves its own live.
///
/// Pinned here as well as end-to-end because the integration path cannot
/// force the interesting interleaving — whether the older scan has already
/// finished by the time the newer one starts is a race, and this is the
/// behavior that has to hold either way.
#[test]
fn a_fresh_generation_cancels_the_previous_one() {
    let shutdown = CancellationToken::new();
    let generation = Generation::default();

    let first = generation.begin(&shutdown);
    assert!(!first.is_cancelled());

    let second = generation.begin(&shutdown);
    assert!(first.is_cancelled(), "the older stream must be superseded");
    assert!(!second.is_cancelled(), "the newer one must stay live");

    let third = generation.begin(&shutdown);
    assert!(second.is_cancelled());
    assert!(!third.is_cancelled());
}

/// Daemon shutdown has to stop a scan that no keystroke ever superseded.
#[test]
fn shutdown_cancels_every_generation_token() {
    let shutdown = CancellationToken::new();
    let generation = Generation::default();
    let token = generation.begin(&shutdown);
    assert!(!token.is_cancelled());
    shutdown.cancel();
    assert!(token.is_cancelled());
}

/// The wire enum reserves 0 for UNSPECIFIED, so its numbers are one higher
/// than `finder_index.kind`'s. Deriving one from the other with a `+ 1` is
/// the off-by-one this mapping exists to prevent, so both are asserted.
#[test]
fn the_wire_kind_numbering_is_one_higher_than_the_storage_codes() {
    for kind in ItemKind::ALL {
        let wire = to_proto_kind(kind) as i32;
        assert_eq!(
            i64::from(wire),
            kind.code() + 1,
            "{kind:?} disagrees between the wire and the schema"
        );
        assert_ne!(wire, ProtoItemKind::Unspecified as i32);
    }
    assert_eq!(
        to_proto_kind(ItemKind::Message) as i32,
        ProtoItemKind::Message as i32
    );
}

/// UNSPECIFIED means "use the server's default", never "search nothing".
#[test]
fn an_unspecified_scope_defers_to_the_server_default() {
    assert_eq!(decode_scope(ProtoScope::Unspecified as i32), None);
    // ...and so does a number from a newer client this build does not know.
    assert_eq!(decode_scope(9_999), None);
}

#[test]
fn every_wire_scope_decodes() {
    for (wire, scope) in [
        (ProtoScope::All, Scope::All),
        (ProtoScope::Messages, Scope::Only(ItemKind::Message)),
        (ProtoScope::Mailboxes, Scope::Only(ItemKind::Mailbox)),
        (ProtoScope::Contacts, Scope::Only(ItemKind::Contact)),
        (
            ProtoScope::SavedSearches,
            Scope::Only(ItemKind::SavedSearch),
        ),
        (ProtoScope::Tags, Scope::Only(ItemKind::Tag)),
        (ProtoScope::Commands, Scope::Only(ItemKind::Command)),
    ] {
        assert_eq!(decode_scope(wire as i32), Some(scope), "{wire:?}");
    }
}

/// `ref_id` is a row id in whichever source table the kind names, and those
/// id spaces overlap: tag 7, mailbox 7 and message 7 all exist. An unstated
/// kind would let a client that forwarded an unfiltered `--scope all`
/// selection archive *message* 7 because the user picked *tag* 7.
#[test]
fn a_batch_action_must_say_its_ids_are_messages() {
    assert!(require_message_kind(ProtoItemKind::Message as i32).is_ok());

    for wrong in [
        ProtoItemKind::Unspecified,
        ProtoItemKind::Mailbox,
        ProtoItemKind::Contact,
        ProtoItemKind::SavedSearch,
        ProtoItemKind::Tag,
        ProtoItemKind::Command,
    ] {
        let status =
            require_message_kind(wrong as i32).expect_err("only message ids may be acted on");
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{wrong:?}");
    }
    // A kind number from a newer client is refused, not defaulted.
    let status = require_message_kind(9_999).expect_err("unknown kind");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

/// The batch cap exists so a client cannot set this handler's runtime — every
/// id is an IMAP round trip, and graceful shutdown is bounded per RPC.
#[test]
fn the_batch_cap_is_above_a_full_page_of_results() {
    assert!(
        MAX_BATCH_REFS >= rmail_core::config::FinderConfig::default().max_results as usize,
        "acting on a whole page of results must always be allowed"
    );
}

#[test]
fn the_action_vocabulary_is_closed() {
    for (verb, action) in [
        ("archive", ItemAction::Archive),
        ("delete", ItemAction::Delete),
        ("read", ItemAction::Read),
        ("unread", ItemAction::Unread),
        ("flag", ItemAction::Flag),
        ("unflag", ItemAction::Unflag),
    ] {
        assert_eq!(decode_action(verb).expect("known verb"), action);
    }
    // An unknown verb is a refusal that names itself, never a silent no-op.
    let status = decode_action("Archive").expect_err("case matters");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    let status = decode_action("").expect_err("empty is not an action");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(decode_action("move").is_err());
}
