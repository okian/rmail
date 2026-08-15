//! The dynamic codec, checked against `prost`'s own generated types.
//!
//! Every round trip here encodes JSON with this module and decodes it with
//! the generated `rmail_proto::v1` struct (or the reverse). A test that only
//! round-tripped through *this* module would agree with itself about a wrong
//! wire format and pass; agreeing with prost is what makes these tests of the
//! protobuf encoding rather than of this module's own inverse.

use super::*;
use prost::Message as _;
use serde_json::json;

fn catalog() -> &'static Catalog {
    crate::mcp::descriptor::catalog().expect("the compiled descriptor set")
}

#[test]
fn a_scalar_encodes_to_what_prost_decodes() {
    let bytes = encode(
        catalog(),
        "rmail.v1.GetMessageRequest",
        &json!({ "id": 42 }),
    )
    .expect("encode");
    let decoded =
        rmail_proto::v1::GetMessageRequest::decode(bytes.as_slice()).expect("prost decodes it");
    assert_eq!(decoded.id, 42);
}

#[test]
fn what_prost_encodes_decodes_back_to_json() {
    let request = rmail_proto::v1::GetMessageRequest { id: 7 };
    let value = decode(
        catalog(),
        "rmail.v1.GetMessageRequest",
        &request.encode_to_vec(),
    )
    .expect("decode");
    assert_eq!(value, json!({ "id": 7 }));
}

#[test]
fn a_string_and_a_repeated_field_round_trip_through_prost() {
    let value = json!({
        "message_id": 5,
        "flags": ["\\Seen", "\\Flagged"],
        "idempotency_key": "abc",
    });
    let bytes = encode(catalog(), "rmail.v1.SetFlagsRequest", &value).expect("encode");
    let decoded =
        rmail_proto::v1::SetFlagsRequest::decode(bytes.as_slice()).expect("prost decodes it");
    assert_eq!(decoded.message_id, 5);
    assert_eq!(
        decoded.flags,
        vec!["\\Seen".to_owned(), "\\Flagged".to_owned()]
    );
    assert_eq!(decoded.idempotency_key, "abc");

    let back = decode(catalog(), "rmail.v1.SetFlagsRequest", &bytes).expect("decode");
    assert_eq!(back, value);
}

/// prost writes packed runs for repeated numeric scalars; this decoder has to
/// read them, and this encoder's unpacked output has to be readable by prost.
/// Both wire forms are legal for the same declaration, so both directions are
/// checked.
#[test]
fn packed_and_unpacked_repeated_scalars_both_decode() {
    let packed = rmail_proto::v1::MessageIds { ids: vec![9, 8, 7] }.encode_to_vec();
    let from_packed = decode(catalog(), "rmail.v1.MessageIds", &packed).expect("packed");
    assert_eq!(from_packed["ids"], json!([9, 8, 7]));

    let unpacked = encode(
        catalog(),
        "rmail.v1.MessageIds",
        &json!({ "ids": [9, 8, 7] }),
    )
    .expect("encode");
    assert_ne!(unpacked, packed, "this encoder writes the unpacked form");
    let reparsed = rmail_proto::v1::MessageIds::decode(unpacked.as_slice()).expect("prost");
    assert_eq!(reparsed.ids, vec![9, 8, 7]);
}

#[test]
fn an_enum_is_spelled_by_name_in_both_directions() {
    let value = json!({ "account_id": 1, "mode": "SYNC_MODE_FULL" });
    let bytes = encode(catalog(), "rmail.v1.SyncFolderRequest", &value).expect("encode");
    let decoded =
        rmail_proto::v1::SyncFolderRequest::decode(bytes.as_slice()).expect("prost decodes it");
    assert_eq!(decoded.mode, rmail_proto::v1::SyncMode::Full as i32);

    let back = decode(catalog(), "rmail.v1.SyncFolderRequest", &bytes).expect("decode");
    assert_eq!(back["mode"], "SYNC_MODE_FULL");
}

#[test]
fn an_unknown_enum_name_is_rejected_and_the_error_lists_the_real_ones() {
    let error = encode(
        catalog(),
        "rmail.v1.SyncFolderRequest",
        &json!({ "mode": "SYNC_MODE_SIDEWAYS" }),
    )
    .expect_err("an invented enum value must not encode");
    let text = error.to_string();
    assert!(text.contains("SYNC_MODE_FULL"), "{text}");
}

/// The failure this codec exists to make impossible: an argument the caller
/// believes is filtering, silently dropped.
#[test]
fn an_unknown_argument_is_an_error_not_a_silent_drop() {
    let error = encode(
        catalog(),
        "rmail.v1.SyncFolderRequest",
        &json!({ "account_id": 1, "account": 3 }),
    )
    .expect_err("an unknown field must be refused");
    assert!(matches!(error, McpError::InvalidArguments(_)), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("\"account\""), "{text}");
    assert!(
        text.contains("account_id"),
        "the error must list the fields that do exist: {text}"
    );
}

#[test]
fn the_camel_case_spelling_of_a_known_field_is_accepted() {
    let bytes = encode(
        catalog(),
        "rmail.v1.SyncFolderRequest",
        &json!({ "accountId": 11 }),
    )
    .expect("proto3 JSON's own spelling is a legal spelling");
    let decoded = rmail_proto::v1::SyncFolderRequest::decode(bytes.as_slice()).expect("prost");
    assert_eq!(decoded.account_id, 11);
}

#[test]
fn a_null_argument_is_absence_not_a_zero() {
    let bytes = encode(
        catalog(),
        "rmail.v1.SyncFolderRequest",
        &json!({ "account_id": 5, "mailbox_id": null }),
    )
    .expect("encode");
    let decoded = rmail_proto::v1::SyncFolderRequest::decode(bytes.as_slice()).expect("prost");
    assert_eq!(decoded.account_id, 5);
    assert_eq!(
        decoded.mailbox_id, None,
        "a null optional must stay absent, not become Some(0)"
    );
}

/// An `optional` field explicitly set to its default must survive as present:
/// that is the whole difference proto3 `optional` buys, and dropping it would
/// turn "sync mailbox 0" into "sync every folder".
#[test]
fn an_optional_field_set_to_zero_stays_present() {
    let bytes = encode(
        catalog(),
        "rmail.v1.SyncFolderRequest",
        &json!({ "account_id": 5, "mailbox_id": 0 }),
    )
    .expect("encode");
    let decoded = rmail_proto::v1::SyncFolderRequest::decode(bytes.as_slice()).expect("prost");
    assert_eq!(decoded.mailbox_id, Some(0));
}

#[test]
fn a_type_mismatch_names_the_field_and_what_it_wanted() {
    let error = encode(
        catalog(),
        "rmail.v1.GetMessageRequest",
        &json!({ "id": "not a number" }),
    )
    .expect_err("a string that is not an integer must be refused");
    let text = error.to_string();
    assert!(text.contains("id"), "{text}");
    assert!(text.contains("integer"), "{text}");
}

/// 64-bit ids are advertised as integers but accepted as the canonical
/// proto3-JSON string too, and are *emitted* as a string past the range a
/// JSON number represents exactly.
#[test]
fn a_64_bit_field_accepts_and_emits_the_string_spelling_past_2_53() {
    let bytes = encode(
        catalog(),
        "rmail.v1.GetMessageRequest",
        &json!({ "id": "9007199254740993" }),
    )
    .expect("encode");
    let decoded = rmail_proto::v1::GetMessageRequest::decode(bytes.as_slice()).expect("prost");
    assert_eq!(decoded.id, 9_007_199_254_740_993);

    let back = decode(catalog(), "rmail.v1.GetMessageRequest", &bytes).expect("decode");
    assert_eq!(back["id"], "9007199254740993");
}

#[test]
fn a_negative_scalar_survives_the_round_trip() {
    let request = rmail_proto::v1::GetMessageRequest { id: -3 };
    let back = decode(
        catalog(),
        "rmail.v1.GetMessageRequest",
        &request.encode_to_vec(),
    )
    .expect("decode");
    assert_eq!(back["id"], -3);
    let bytes = encode(catalog(), "rmail.v1.GetMessageRequest", &back).expect("encode");
    assert_eq!(
        rmail_proto::v1::GetMessageRequest::decode(bytes.as_slice())
            .expect("prost")
            .id,
        -3
    );
}

#[test]
fn a_nested_message_round_trips_and_absent_fields_stay_absent() {
    let value = json!({
        "account_id": 0,
        "class": "BUDGET_CLASS_BULK",
        "caps": {
            "daily": { "hard_usd": 12.5, "soft_tokens": 1000 },
        },
    });
    let bytes = encode(catalog(), "rmail.v1.SetBudgetRequest", &value).expect("encode");
    let decoded =
        rmail_proto::v1::SetBudgetRequest::decode(bytes.as_slice()).expect("prost decodes it");
    let daily = decoded.caps.expect("caps").daily.expect("daily");
    assert_eq!(daily.hard_usd, Some(12.5));
    assert_eq!(daily.soft_tokens, Some(1000));
    assert_eq!(daily.hard_tokens, None);

    let back = decode(catalog(), "rmail.v1.SetBudgetRequest", &bytes).expect("decode");
    assert_eq!(back, value, "the whole object must survive the round trip");
    assert!(
        back["caps"]["daily"].get("hard_tokens").is_none(),
        "a field the caller did not send must stay absent rather than come back as a zero"
    );
    // `account_id: 0` *does* come back, and that is deliberate: this encoder
    // writes every field the caller sent, default-valued or not. Omitting
    // defaults would be canonical proto3 — and would silently drop the
    // explicit `0` on an `optional` field, which is the one place proto3 can
    // tell "unset" from "zero" and where dropping it turns "sync mailbox 0"
    // into "sync every folder" (see `an_optional_field_set_to_zero_stays_present`).
    // One rule for both kinds of field is worth the handful of extra bytes.
    assert_eq!(back["account_id"], 0);
}

/// `google.protobuf.Empty` is what half the mutating RPCs return.
#[test]
fn an_empty_response_decodes_to_an_empty_object() {
    assert_eq!(
        decode(catalog(), "google.protobuf.Empty", &[]).expect("decode"),
        json!({})
    );
}

#[test]
fn a_field_this_build_does_not_know_is_skipped_rather_than_fatal() {
    let mut bytes =
        encode(catalog(), "rmail.v1.GetMessageRequest", &json!({ "id": 2 })).expect("encode");
    // Field 9999, length-delimited, three bytes — a field no rmail.v1 message
    // declares.
    put_key(9999, Wire::LengthDelimited, &mut bytes);
    put_varint(3, &mut bytes);
    bytes.extend_from_slice(b"abc");

    let value =
        decode(catalog(), "rmail.v1.GetMessageRequest", &bytes).expect("an unknown field is fine");
    assert_eq!(value, json!({ "id": 2 }));
}

#[test]
fn a_truncated_body_is_an_error_not_a_panic() {
    let bytes = encode(
        catalog(),
        "rmail.v1.GetMessageRequest",
        &json!({ "id": 300 }),
    )
    .expect("encode");
    let truncated = &bytes[..bytes.len() - 1];
    let error = decode(catalog(), "rmail.v1.GetMessageRequest", truncated)
        .expect_err("a truncated varint must be reported");
    assert!(matches!(error, McpError::Wire(_)), "{error:?}");
}

#[test]
fn a_length_prefix_past_the_end_of_the_buffer_is_an_error() {
    // Field 3 (`idempotency_key`, a string) claiming 200 bytes it does not
    // have.
    let mut bytes = Vec::new();
    put_key(3, Wire::LengthDelimited, &mut bytes);
    put_varint(200, &mut bytes);
    bytes.extend_from_slice(b"short");
    let error = decode(catalog(), "rmail.v1.SetFlagsRequest", &bytes)
        .expect_err("an over-long length prefix must be reported");
    assert!(matches!(error, McpError::Wire(_)), "{error:?}");
}

#[test]
fn a_varint_that_never_terminates_is_an_error() {
    let error = decode(catalog(), "rmail.v1.GetMessageRequest", &[0xff; 12])
        .expect_err("an eleven-byte varint is not a varint");
    assert!(matches!(error, McpError::Wire(_)), "{error:?}");
}

#[test]
fn arguments_that_are_not_an_object_are_refused() {
    let error = encode(catalog(), "rmail.v1.GetMessageRequest", &json!([1, 2, 3]))
        .expect_err("an array is not an argument object");
    assert!(matches!(error, McpError::InvalidArguments(_)), "{error:?}");
}

#[test]
fn a_repeated_field_given_a_scalar_is_refused() {
    let error = encode(
        catalog(),
        "rmail.v1.SetFlagsRequest",
        &json!({ "flags": "\\Seen" }),
    )
    .expect_err("a repeated field needs an array");
    let text = error.to_string();
    assert!(text.contains("flags"), "{text}");
    assert!(text.contains("array"), "{text}");
}

/// The nesting bound exists for hostile input, so it has to actually stop
/// something rather than merely be documented.
#[test]
fn deeply_nested_arguments_are_refused_rather_than_overflowing_the_stack() {
    let mut value = json!({});
    for _ in 0..(MAX_NESTING + 5) {
        value = json!({ "caps": value });
    }
    let error = encode(catalog(), "rmail.v1.SetBudgetRequest", &value)
        .expect_err("a deeply nested argument object must be refused");
    // Refused either for depth or because `BudgetCaps` has no `caps` field —
    // both are refusals rather than a stack overflow, which is the property.
    assert!(matches!(error, McpError::InvalidArguments(_)), "{error:?}");
}

/// Both halves of the 64-bit rendering rule, which is the one place this
/// codec deviates from canonical proto3 JSON on purpose.
#[test]
fn int64_renders_as_a_number_inside_the_exact_range_and_a_string_outside_it() {
    assert_eq!(
        int64(9_007_199_254_740_992),
        json!(9_007_199_254_740_992_i64)
    );
    assert_eq!(int64(9_007_199_254_740_993), json!("9007199254740993"));
    assert_eq!(int64(-9_007_199_254_740_993), json!("-9007199254740993"));
    assert_eq!(uint64(1), json!(1));
    assert_eq!(uint64(u64::MAX), json!("18446744073709551615"));
}

/// Two arms of one `oneof` in the same argument object must be refused.
///
/// Both encode fine and the decoder then keeps whichever arrived last, so a
/// `bulk_tag {message_ids, query}` would apply a bulk mutation to a
/// query-selected set while the caller believed it had named the messages
/// explicitly — the same class of silent wrong-target write this module
/// refuses unknown keys to prevent.
#[test]
fn two_arms_of_one_oneof_are_refused_rather_than_silently_resolved() {
    let error = encode(
        catalog(),
        "rmail.v1.BulkTagRequest",
        &json!({
            "account_id": 1,
            "query": "from:alice",
            "message_ids": { "ids": [1, 2] },
            "names": ["urgent"],
        }),
    )
    .expect_err("two arms of `oneof selector` must not both encode");
    let text = error.to_string();
    assert!(text.contains("query"), "{text}");
    assert!(text.contains("message_ids"), "{text}");
    assert!(
        text.contains("selector"),
        "the error must name the group: {text}"
    );
}

/// Either arm alone is fine, so the check is exclusivity rather than a ban.
#[test]
fn one_arm_of_a_oneof_encodes_normally() {
    for arm in [
        json!({ "account_id": 1, "query": "from:alice", "names": ["urgent"] }),
        json!({ "account_id": 1, "message_ids": { "ids": [1, 2] }, "names": ["urgent"] }),
    ] {
        assert!(
            encode(catalog(), "rmail.v1.BulkTagRequest", &arm).is_ok(),
            "{arm} should encode"
        );
    }
}

/// A proto3 `optional` field is a synthetic single-member oneof, so it must
/// not be reported as conflicting with itself.
#[test]
fn a_proto3_optional_is_not_treated_as_a_oneof_arm() {
    assert!(encode(
        catalog(),
        "rmail.v1.SyncFolderRequest",
        &json!({ "account_id": 1, "mailbox_id": 2 }),
    )
    .is_ok());
}

/// An explicit `null` on one arm leaves the other free — absence is absence.
#[test]
fn a_null_arm_does_not_conflict_with_a_real_one() {
    assert!(encode(
        catalog(),
        "rmail.v1.BulkTagRequest",
        &json!({ "account_id": 1, "query": "from:alice", "message_ids": null, "names": ["x"] }),
    )
    .is_ok());
}

/// A map field arriving with the wrong wire type is an error, not a silent
/// mis-parse: every non-map field's wire type is validated in `decode_single`,
/// and this closes the one path that skipped it.
#[test]
fn a_map_field_with_the_wrong_wire_type_is_refused() {
    let catalog = crate::mcp::descriptor::Catalog::build(&map_descriptor_set().encode_to_vec())
        .expect("synthetic catalog");
    // Field 1 (`labels`, a map) sent as a varint.
    let mut bytes = Vec::new();
    put_key(1, Wire::Varint, &mut bytes);
    put_varint(7, &mut bytes);
    let error =
        decode(&catalog, "t.Tagged", &bytes).expect_err("a map must arrive length-delimited");
    let text = error.to_string();
    // The *message* matters, not just that it errored: without the explicit
    // check the varint is read as a length prefix and `take_slice` complains
    // about a buffer overrun instead — an error either way, but one that sends
    // a reader looking for a truncated message rather than a mistyped field.
    assert!(
        text.contains("length-delimited") && text.contains("labels"),
        "{text}"
    );
}

/// A map field, built from a synthetic descriptor: no `rmail.v1` message has
/// one today, and a code path with no test is one that will be wrong the day
/// a proto grows one.
#[test]
fn a_map_field_round_trips_through_a_synthetic_descriptor() {
    let catalog = crate::mcp::descriptor::Catalog::build(&map_descriptor_set().encode_to_vec())
        .expect("synthetic catalog");

    let value = json!({ "labels": { "later": 2, "urgent": 1 } });
    let bytes = encode(&catalog, "t.Tagged", &value).expect("encode");
    let back = decode(&catalog, "t.Tagged", &bytes).expect("decode");
    assert_eq!(back, value);

    // ...and the schema calls it an object keyed by string, not an array.
    let schema = crate::mcp::schema::input_schema(&catalog, "t.Tagged").expect("schema");
    assert_eq!(schema["properties"]["labels"]["type"], "object");
    assert_eq!(
        schema["properties"]["labels"]["additionalProperties"]["format"],
        "int64"
    );
}

/// `t.Tagged { map<string, int64> labels = 1; }`, as a descriptor set.
fn map_descriptor_set() -> prost_types::FileDescriptorSet {
    use prost_types::field_descriptor_proto::{Label, Type};
    use prost_types::{
        DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
        MessageOptions, MethodDescriptorProto, ServiceDescriptorProto,
    };

    fn scalar(name: &str, number: i32, ty: Type) -> FieldDescriptorProto {
        FieldDescriptorProto {
            name: Some(name.to_owned()),
            number: Some(number),
            label: Some(Label::Optional as i32),
            r#type: Some(ty as i32),
            json_name: Some(name.to_owned()),
            ..Default::default()
        }
    }

    let entry = DescriptorProto {
        name: Some("LabelsEntry".to_owned()),
        field: vec![
            scalar("key", 1, Type::String),
            scalar("value", 2, Type::Int64),
        ],
        options: Some(MessageOptions {
            map_entry: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let message = DescriptorProto {
        name: Some("Tagged".to_owned()),
        field: vec![FieldDescriptorProto {
            name: Some("labels".to_owned()),
            number: Some(1),
            label: Some(Label::Repeated as i32),
            r#type: Some(Type::Message as i32),
            type_name: Some(".t.Tagged.LabelsEntry".to_owned()),
            json_name: Some("labels".to_owned()),
            ..Default::default()
        }],
        nested_type: vec![entry],
        ..Default::default()
    };
    FileDescriptorSet {
        file: vec![FileDescriptorProto {
            name: Some("t.proto".to_owned()),
            package: Some("t".to_owned()),
            message_type: vec![message],
            service: vec![ServiceDescriptorProto {
                name: Some("S".to_owned()),
                method: vec![MethodDescriptorProto {
                    name: Some("M".to_owned()),
                    input_type: Some(".t.Tagged".to_owned()),
                    output_type: Some(".t.Tagged".to_owned()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}
