//! The "arg mapping" half of task 53: a request message's own fields, turned
//! into the JSON Schema an MCP tool advertises as its `inputSchema`.
//!
//! # Generated, never listed
//!
//! prd.md's design invariant only holds if a new RPC becomes a *usable* tool
//! with no extra code, and an agent cannot use a tool whose arguments nobody
//! described. So the schema is derived from
//! `DescriptorProto`/`FieldDescriptorProto` — the same bytes that describe the
//! request on the wire — rather than hand-written per tool. A hand-written
//! table would drift the first time a field was added, and the drift would be
//! silent: the agent would simply stop being able to pass the new argument.
//!
//! # Strict on purpose
//!
//! Every generated object carries `additionalProperties: false`, and
//! [`super::codec`] enforces the same rule when decoding a call's arguments.
//! Accepting an unrecognised key and dropping it is the failure mode that
//! matters here: an agent that passes `account` where the field is
//! `account_id` would get a mailbox-wide answer that looks like a filtered
//! one, with nothing anywhere saying the filter was ignored.
//!
//! Properties are keyed by the proto field name (`top_k`) rather than the
//! camelCase `json_name` (`topK`) — see [`super::codec`]'s module docs for
//! why. The codec accepts either spelling on input even though only one is
//! advertised, so `additionalProperties: false` is a statement about
//! *unknown* fields, not about which of the two legal spellings of a known
//! one a client chose.
//!
//! # Depth, not `$ref`
//!
//! Message-typed fields are expanded inline rather than emitted as
//! `$defs`/`$ref`: MCP clients vary in how much of JSON Schema they resolve,
//! and a tool list is read by a model, which follows a concrete nested object
//! far more reliably than a pointer. Expansion is bounded by
//! [`MAX_DEPTH`] and by a cycle guard over the types currently being
//! expanded, so a self-referential message (none today) yields a truncated
//! node describing itself rather than an infinite document.

use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FieldDescriptorProto};
use serde_json::{json, Map, Value};

use super::descriptor::Catalog;
use super::McpError;

/// How deep a message-typed field is expanded before the schema truncates.
///
/// Six is well past the deepest request in `proto/rmail/v1` (three), and is a
/// bound on document size rather than a statement about the protos: a tool
/// list is sent to a model on every session, so an accidentally recursive
/// request message must cost a paragraph, not a megabyte.
const MAX_DEPTH: usize = 6;

/// The JSON Schema for `message_name`'s fields — an MCP tool's `inputSchema`.
///
/// # Errors
///
/// [`McpError::Descriptor`] if the message, or any type it references, is not
/// in the compiled descriptor set. That is a malformed descriptor set rather
/// than a bad request, since every `type_name` a field carries was written by
/// `protoc` from a resolved import.
pub fn input_schema(catalog: &Catalog, message_name: &str) -> Result<Value, McpError> {
    let mut stack = Vec::new();
    message_schema(catalog, message_name, 0, &mut stack)
}

/// The schema for one message type.
fn message_schema(
    catalog: &Catalog,
    message_name: &str,
    depth: usize,
    stack: &mut Vec<String>,
) -> Result<Value, McpError> {
    let name = message_name.trim_start_matches('.');
    let message = catalog.message(name).ok_or_else(|| {
        McpError::Descriptor(format!("no message named {name} in the descriptor set"))
    })?;

    stack.push(name.to_owned());
    let result = build_object(catalog, message, depth, stack);
    stack.pop();
    result
}

/// The object schema for an already-resolved message descriptor.
fn build_object(
    catalog: &Catalog,
    message: &DescriptorProto,
    depth: usize,
    stack: &mut Vec<String>,
) -> Result<Value, McpError> {
    let mut properties = Map::new();
    for field in &message.field {
        properties.insert(
            field.name().to_owned(),
            field_schema(catalog, field, depth, stack)?,
        );
    }

    let mut object = Map::new();
    object.insert("type".to_owned(), json!("object"));
    object.insert("properties".to_owned(), Value::Object(properties));
    // Every proto3 field is optional on the wire, so nothing is `required`.
    // Saying so explicitly (rather than omitting the keyword) keeps a client
    // that fills required fields eagerly from inventing values for a filter
    // the caller deliberately left off.
    object.insert("required".to_owned(), json!([]));
    object.insert("additionalProperties".to_owned(), json!(false));
    if let Some(description) = oneof_description(message) {
        object.insert("description".to_owned(), json!(description));
    }
    Ok(Value::Object(object))
}

/// "At most one of ..." for each real `oneof` in the message.
///
/// proto3's `optional` keyword is implemented as a synthetic single-field
/// oneof, which carries no mutual exclusion for a caller to respect — listing
/// those would tell an agent that a plain optional field conflicts with
/// itself. `proto3_optional` is exactly the flag that distinguishes them.
fn oneof_description(message: &DescriptorProto) -> Option<String> {
    let mut groups: Vec<String> = Vec::new();
    for (index, oneof) in message.oneof_decl.iter().enumerate() {
        // `oneof_index` is an `i32` on the wire; a message with more than
        // `i32::MAX` oneofs cannot exist, so this is a total conversion
        // written as a fallible one rather than a cast that would need an
        // `#[allow]`.
        let Ok(index) = i32::try_from(index) else {
            continue;
        };
        let members: Vec<&str> = message
            .field
            .iter()
            .filter(|f| f.oneof_index == Some(index) && !f.proto3_optional())
            .map(FieldDescriptorProto::name)
            .collect();
        if members.len() > 1 {
            groups.push(format!("{}: {}", oneof.name(), members.join(", ")));
        }
    }
    if groups.is_empty() {
        None
    } else {
        Some(format!(
            "Set at most one field of each group — {}.",
            groups.join("; ")
        ))
    }
}

/// The schema for one field, including its repeated/map wrapping.
fn field_schema(
    catalog: &Catalog,
    field: &FieldDescriptorProto,
    depth: usize,
    stack: &mut Vec<String>,
) -> Result<Value, McpError> {
    if field.label() == Label::Repeated {
        // A `map<K, V>` is a repeated field of a synthetic entry message, and
        // is the one case where "repeated" must not become a JSON array —
        // proto3 JSON renders it as an object keyed by the stringified key.
        if let Some(entry) = map_entry(catalog, field) {
            let value_field = entry
                .field
                .iter()
                .find(|f| f.number() == 2)
                .ok_or_else(|| {
                    McpError::Descriptor(format!(
                        "map entry for {} has no value field",
                        field.name()
                    ))
                })?;
            return Ok(json!({
                "type": "object",
                "additionalProperties": field_scalar_schema(catalog, value_field, depth, stack)?,
            }));
        }
        return Ok(json!({
            "type": "array",
            "items": field_scalar_schema(catalog, field, depth, stack)?,
        }));
    }
    field_scalar_schema(catalog, field, depth, stack)
}

/// The schema for a field's element type, ignoring its label.
fn field_scalar_schema(
    catalog: &Catalog,
    field: &FieldDescriptorProto,
    depth: usize,
    stack: &mut Vec<String>,
) -> Result<Value, McpError> {
    Ok(match field.r#type() {
        Type::Double | Type::Float => json!({ "type": "number" }),
        Type::Int32 | Type::Sint32 | Type::Sfixed32 => {
            json!({ "type": "integer", "format": "int32" })
        }
        Type::Uint32 | Type::Fixed32 => json!({ "type": "integer", "format": "uint32" }),
        // Declared as an integer, and accepted as a JSON string too (see
        // `codec`): proto3 JSON's canonical form for 64-bit integers is a
        // string, because values above 2^53 do not survive a JSON number.
        // Advertising `integer` is what makes a model emit `12` rather than
        // `"12"` for the message ids that make up almost every argument here.
        Type::Int64 | Type::Sint64 | Type::Sfixed64 => {
            json!({ "type": "integer", "format": "int64" })
        }
        Type::Uint64 | Type::Fixed64 => json!({ "type": "integer", "format": "uint64" }),
        Type::Bool => json!({ "type": "boolean" }),
        Type::String => json!({ "type": "string" }),
        Type::Bytes => json!({ "type": "string", "contentEncoding": "base64" }),
        Type::Enum => {
            let name = field.type_name().trim_start_matches('.');
            let enumeration = catalog.enumeration(name).ok_or_else(|| {
                McpError::Descriptor(format!("no enum named {name} in the descriptor set"))
            })?;
            let names: Vec<&str> = enumeration.value.iter().map(|v| v.name()).collect();
            json!({ "type": "string", "enum": names })
        }
        Type::Message | Type::Group => {
            let name = field.type_name().trim_start_matches('.');
            if depth >= MAX_DEPTH || stack.iter().any(|seen| seen == name) {
                json!({
                    "type": "object",
                    "description": format!(
                        "{name}; not expanded further (recursive or deeper than {MAX_DEPTH} \
                         levels). Use the gRPC reflection service for its full shape."
                    ),
                })
            } else {
                message_schema(catalog, name, depth + 1, stack)?
            }
        }
    })
}

/// The synthetic entry descriptor behind a `map<K, V>` field, if this field is
/// one.
pub fn map_entry<'a>(
    catalog: &'a Catalog,
    field: &FieldDescriptorProto,
) -> Option<&'a DescriptorProto> {
    if field.r#type() != Type::Message {
        return None;
    }
    let entry = catalog.message(field.type_name())?;
    entry
        .options
        .as_ref()
        .and_then(|o| o.map_entry)
        .unwrap_or(false)
        .then_some(entry)
}

#[cfg(test)]
mod tests;
