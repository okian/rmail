//! The generated `inputSchema`, checked against the protos it comes from.

use super::*;
use crate::mcp::descriptor::catalog;

fn schema_for(message: &str) -> Value {
    input_schema(catalog().expect("catalog"), message).expect("schema")
}

#[test]
fn a_scalar_field_is_typed_from_the_descriptor() {
    let schema = schema_for("rmail.v1.SyncFolderRequest");
    assert_eq!(schema["properties"]["account_id"]["type"], "integer");
    assert_eq!(schema["properties"]["account_id"]["format"], "int64");
    assert_eq!(schema["properties"]["mailbox_id"]["type"], "integer");
}

#[test]
fn an_enum_field_advertises_its_value_names() {
    let schema = schema_for("rmail.v1.SyncFolderRequest");
    let mode = &schema["properties"]["mode"];
    assert_eq!(mode["type"], "string");
    let names: Vec<&str> = mode["enum"]
        .as_array()
        .expect("an enum list")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(names.contains(&"SYNC_MODE_FULL"), "{names:?}");
    assert!(names.contains(&"SYNC_MODE_AUTO"), "{names:?}");
}

#[test]
fn a_repeated_field_is_an_array_of_its_element_type() {
    let schema = schema_for("rmail.v1.SetFlagsRequest");
    let flags = &schema["properties"]["flags"];
    assert_eq!(flags["type"], "array");
    assert_eq!(flags["items"]["type"], "string");
}

#[test]
fn a_message_field_is_expanded_inline() {
    let schema = schema_for("rmail.v1.SetBudgetRequest");
    let daily = &schema["properties"]["caps"]["properties"]["daily"];
    assert_eq!(daily["type"], "object");
    assert_eq!(daily["properties"]["hard_usd"]["type"], "number");
    assert_eq!(daily["properties"]["hard_tokens"]["format"], "int64");
}

/// `additionalProperties: false` is the advertised half of the codec's refusal
/// to drop an unrecognised argument.
#[test]
fn every_object_refuses_unknown_properties() {
    fn walk(value: &Value, path: &str) {
        if value["type"] == "object" && value.get("properties").is_some() {
            assert_eq!(
                value["additionalProperties"],
                Value::Bool(false),
                "{path} accepts unknown properties"
            );
        }
        if let Some(properties) = value.get("properties").and_then(Value::as_object) {
            for (name, child) in properties {
                walk(child, &format!("{path}.{name}"));
            }
        }
        if let Some(items) = value.get("items") {
            walk(items, &format!("{path}[]"));
        }
    }
    let catalog = catalog().expect("catalog");
    for method in catalog.methods() {
        let schema = input_schema(catalog, &method.input_type).expect("schema");
        walk(&schema, &method.path);
    }
}

/// proto3 has no required fields, and saying so keeps a client from inventing
/// a value for a filter the caller deliberately left off.
#[test]
fn nothing_is_declared_required() {
    let catalog = catalog().expect("catalog");
    for method in catalog.methods() {
        let schema = input_schema(catalog, &method.input_type).expect("schema");
        assert_eq!(
            schema["required"],
            serde_json::json!([]),
            "{} declares required fields",
            method.path
        );
    }
}

/// A real `oneof` is described so an agent knows the fields conflict; a
/// proto3 `optional` — which is a synthetic single-member oneof — must not be.
#[test]
fn a_real_oneof_is_described_and_a_proto3_optional_is_not() {
    let tagged = schema_for("rmail.v1.BulkTagRequest");
    let description = tagged["description"].as_str().unwrap_or_default();
    assert!(
        description.contains("selector"),
        "BulkTagRequest's `oneof selector` must be described: {tagged}"
    );

    // `SyncFolderRequest.mailbox_id` is `optional int64`, i.e. a synthetic
    // one-member oneof. Describing it would tell an agent a field conflicts
    // with itself.
    let sync = schema_for("rmail.v1.SyncFolderRequest");
    assert!(
        sync.get("description").is_none(),
        "a proto3 optional must not be reported as a oneof group: {sync}"
    );
}

#[test]
fn every_served_request_message_produces_a_schema() {
    let catalog = catalog().expect("catalog");
    for method in catalog.methods() {
        let schema = input_schema(catalog, &method.input_type);
        assert!(
            schema.is_ok(),
            "{} has no derivable schema: {:?}",
            method.path,
            schema.err()
        );
        assert_eq!(
            schema.unwrap_or_default()["type"],
            "object",
            "{}",
            method.path
        );
    }
}

#[test]
fn an_unknown_message_is_an_error_not_a_panic() {
    let error = input_schema(catalog().expect("catalog"), "rmail.v1.NoSuchRequest")
        .expect_err("an unknown message has no schema");
    assert!(matches!(error, McpError::Descriptor(_)), "{error:?}");
}

/// The depth bound has to actually truncate, or a recursive message would
/// produce an unbounded document on every `tools/list`.
#[test]
fn expansion_stops_at_a_self_referential_message() {
    use prost_types::field_descriptor_proto::{Label, Type};
    use prost_types::{
        DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
    };

    let node = DescriptorProto {
        name: Some("Node".to_owned()),
        field: vec![FieldDescriptorProto {
            name: Some("child".to_owned()),
            number: Some(1),
            label: Some(Label::Optional as i32),
            r#type: Some(Type::Message as i32),
            type_name: Some(".r.Node".to_owned()),
            json_name: Some("child".to_owned()),
            ..Default::default()
        }],
        ..Default::default()
    };
    let set = FileDescriptorSet {
        file: vec![FileDescriptorProto {
            name: Some("r.proto".to_owned()),
            package: Some("r".to_owned()),
            message_type: vec![node],
            service: vec![prost_types::ServiceDescriptorProto {
                name: Some("S".to_owned()),
                method: vec![prost_types::MethodDescriptorProto {
                    name: Some("M".to_owned()),
                    input_type: Some(".r.Node".to_owned()),
                    output_type: Some(".r.Node".to_owned()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    };
    let bytes = {
        use prost::Message as _;
        set.encode_to_vec()
    };
    let catalog = crate::mcp::descriptor::Catalog::build(&bytes).expect("synthetic catalog");

    let schema = input_schema(&catalog, "r.Node").expect("a recursive message still yields one");
    let child = &schema["properties"]["child"];
    assert_eq!(child["type"], "object");
    assert!(
        child["description"]
            .as_str()
            .unwrap_or_default()
            .contains("not expanded further"),
        "the cycle must be truncated with an explanation: {schema}"
    );
    assert!(
        child.get("properties").is_none(),
        "a truncated node must not keep expanding: {schema}"
    );
}
