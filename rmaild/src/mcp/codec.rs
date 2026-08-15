//! A dynamic protobuf <-> JSON codec driven by the compiled descriptor set.
//!
//! This is what makes "a new RPC yields a new tool with zero extra code" true
//! rather than aspirational. A projection that generated tool *names* from the
//! descriptor set but dispatched through hand-written per-RPC glue would ship
//! a tool list whose newest entry could not be called — the drift task 41
//! exists to prevent, moved one layer down.
//!
//! # Why not `prost`'s generated types
//!
//! The generated `rmail_proto::v1` structs are exactly the types a *static*
//! caller wants, and exactly the wrong ones here: dispatching to them requires
//! naming each one, which is the per-RPC code this module exists to avoid.
//! What the projection has instead is a method's `input_type` string, so it
//! encodes from the descriptor and hands tonic opaque bytes (see
//! [`super::invoke`]).
//!
//! # Wire format by hand
//!
//! Varints, keys and length prefixes are written and read here rather than
//! through `prost::encoding`. The format is a page of specification that has
//! not changed in fifteen years, and going through it directly keeps this
//! module's behaviour independent of which low-level helpers a prost minor
//! release chooses to expose. Groups (proto2's deprecated nesting) are
//! rejected rather than half-supported; nothing in `proto/rmail/v1` uses them
//! and a silently-skipped group would corrupt a message body.
//!
//! # JSON shape
//!
//! Keys are the proto field names (`top_k`), not the camelCase `json_name`
//! (`topK`), because that is the spelling prd.md's own MCP tool signatures use
//! (`ask_mailbox {question, top_k?, filter?}`) and the spelling every other
//! JSON surface in this workspace emits. The camelCase form is accepted on
//! input as well — it is the canonical proto3 JSON spelling, so a client that
//! sends it is right rather than confused.
//!
//! An unrecognised key is an **error**, not something to drop. An agent that
//! passes `account` where the field is `account_id` would otherwise get a
//! mailbox-wide answer that looks like a filtered one, with nothing anywhere
//! saying the filter was ignored.

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FieldDescriptorProto};
use serde_json::{Map, Number, Value};

use super::descriptor::Catalog;
use super::schema::map_entry;
use super::McpError;

/// How deep a message may nest before encoding or decoding refuses.
///
/// Both directions are recursive over caller-supplied data — the arguments of
/// a `tools/call` on the way in, a response body on the way out — so both need
/// a bound that is not the stack. Well past anything `proto/rmail/v1`
/// declares; low enough that a hostile payload hits it long before a
/// segmentation fault.
const MAX_NESTING: usize = 32;

/// Protobuf wire types, as the low three bits of a field key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    Varint,
    SixtyFourBit,
    LengthDelimited,
    ThirtyTwoBit,
}

impl Wire {
    fn from_bits(bits: u64) -> Result<Self, McpError> {
        match bits {
            0 => Ok(Wire::Varint),
            1 => Ok(Wire::SixtyFourBit),
            2 => Ok(Wire::LengthDelimited),
            5 => Ok(Wire::ThirtyTwoBit),
            3 | 4 => Err(McpError::Wire(
                "protobuf groups are not supported".to_owned(),
            )),
            other => Err(McpError::Wire(format!("unknown wire type {other}"))),
        }
    }

    fn bits(self) -> u64 {
        match self {
            Wire::Varint => 0,
            Wire::SixtyFourBit => 1,
            Wire::LengthDelimited => 2,
            Wire::ThirtyTwoBit => 5,
        }
    }
}

/// The wire type a field of this protobuf type uses when written singly.
fn wire_for(ty: Type) -> Wire {
    match ty {
        Type::Double | Type::Fixed64 | Type::Sfixed64 => Wire::SixtyFourBit,
        Type::Float | Type::Fixed32 | Type::Sfixed32 => Wire::ThirtyTwoBit,
        Type::String | Type::Bytes | Type::Message | Type::Group => Wire::LengthDelimited,
        Type::Int32
        | Type::Int64
        | Type::Uint32
        | Type::Uint64
        | Type::Sint32
        | Type::Sint64
        | Type::Bool
        | Type::Enum => Wire::Varint,
    }
}

// ---------------------------------------------------------------------------
// Encoding: JSON -> protobuf
// ---------------------------------------------------------------------------

/// Encode `value` as the protobuf wire form of `message_name`.
///
/// Every field the caller *sent* is written, including one whose value is the
/// type's default. Canonical proto3 encoding omits defaults, and omitting them
/// here would be wrong in the one place it matters: a proto3 `optional` field
/// is the only kind that can distinguish "unset" from "zero", and dropping an
/// explicit `mailbox_id: 0` turns "sync mailbox 0" into "sync every folder".
/// One rule for both kinds of field costs a handful of bytes on a request
/// message and removes a class of silent misinterpretation. `null` still means
/// absent — see [`encode_message`].
///
/// # Errors
///
/// [`McpError::InvalidArguments`] if `value` is not an object, names a field
/// the message does not have, or gives one a value of the wrong shape;
/// [`McpError::Descriptor`] if a referenced type is missing from the compiled
/// descriptor set.
pub fn encode(catalog: &Catalog, message_name: &str, value: &Value) -> Result<Vec<u8>, McpError> {
    let mut out = Vec::new();
    encode_message(catalog, message_name, value, 0, &mut out)?;
    Ok(out)
}

fn encode_message(
    catalog: &Catalog,
    message_name: &str,
    value: &Value,
    depth: usize,
    out: &mut Vec<u8>,
) -> Result<(), McpError> {
    if depth > MAX_NESTING {
        return Err(McpError::InvalidArguments(format!(
            "arguments nest deeper than {MAX_NESTING} messages"
        )));
    }
    let name = message_name.trim_start_matches('.');
    let message = catalog.message(name).ok_or_else(|| {
        McpError::Descriptor(format!("no message named {name} in the descriptor set"))
    })?;
    let object = value.as_object().ok_or_else(|| {
        McpError::InvalidArguments(format!("{name} must be a JSON object, got {}", kind(value)))
    })?;

    // Both spellings resolve to the same field; see the module docs.
    let mut by_name: HashMap<&str, &FieldDescriptorProto> = HashMap::new();
    for field in &message.field {
        by_name.insert(field.name(), field);
        by_name.insert(field.json_name(), field);
    }

    // Which `oneof` groups have already been written, and by which field.
    // Two members of one group both encode fine and the *decoder* then keeps
    // whichever arrived last — so `bulk_tag {message_ids, query}` would apply
    // a bulk mutation to a query-selected set while the caller believed it had
    // named the messages explicitly. That is the same class of silent
    // wrong-target write this module refuses unknown keys to prevent, so it is
    // refused here rather than described in prose and hoped for.
    let mut oneof_written: HashMap<i32, &str> = HashMap::new();

    for (key, entry) in object {
        let field = by_name.get(key.as_str()).copied().ok_or_else(|| {
            let mut known: Vec<&str> = message.field.iter().map(|f| f.name()).collect();
            known.sort_unstable();
            McpError::InvalidArguments(format!(
                "{name} has no field {key:?}; known fields: {}",
                known.join(", ")
            ))
        })?;
        // `null` is how JSON says "absent", and proto3 has no way to carry an
        // explicitly-null scalar. Writing the field's default instead would
        // turn "I am not filtering on this" into "filter on 0". Checked before
        // the oneof bookkeeping below, so an explicit `null` on one arm does
        // not conflict with a real value on another.
        if entry.is_null() {
            continue;
        }
        // `proto3_optional` fields are synthetic single-member oneofs and
        // conflict with nothing — including themselves.
        if let Some(index) = field.oneof_index.filter(|_| !field.proto3_optional()) {
            if let Some(first) = oneof_written.insert(index, field.name()) {
                let group = usize::try_from(index)
                    .ok()
                    .and_then(|i| message.oneof_decl.get(i))
                    .map_or("", prost_types::OneofDescriptorProto::name);
                return Err(McpError::InvalidArguments(format!(
                    "{name} sets both {first:?} and {:?}, which are alternatives of the same \
                     oneof ({group}); send exactly one",
                    field.name()
                )));
            }
        }
        encode_field(catalog, message, field, entry, depth, out)?;
    }
    Ok(())
}

fn encode_field(
    catalog: &Catalog,
    message: &DescriptorProto,
    field: &FieldDescriptorProto,
    value: &Value,
    depth: usize,
    out: &mut Vec<u8>,
) -> Result<(), McpError> {
    let tag = u64::from(field.number().unsigned_abs());

    if field.label() == Label::Repeated {
        if let Some(entry) = map_entry(catalog, field) {
            return encode_map(catalog, field, entry, value, depth, out);
        }
        let items = value.as_array().ok_or_else(|| {
            McpError::InvalidArguments(format!(
                "{}.{} is repeated and must be a JSON array, got {}",
                message.name(),
                field.name(),
                kind(value)
            ))
        })?;
        for item in items {
            // Written unpacked even for numeric scalars, which every protobuf
            // decoder must accept for a packable field (the spec requires it,
            // and prost's `merge_repeated` implements it). One code path is
            // worth more here than the handful of bytes packing would save on
            // request messages whose repeated fields are ids and flag names.
            encode_single(catalog, field, item, tag, depth, out)?;
        }
        return Ok(());
    }
    encode_single(catalog, field, value, tag, depth, out)
}

/// A `map<K, V>` field: one length-delimited entry message per pair, with the
/// key in field 1 and the value in field 2.
fn encode_map(
    catalog: &Catalog,
    field: &FieldDescriptorProto,
    entry: &DescriptorProto,
    value: &Value,
    depth: usize,
    out: &mut Vec<u8>,
) -> Result<(), McpError> {
    let object = value.as_object().ok_or_else(|| {
        McpError::InvalidArguments(format!(
            "{} is a map and must be a JSON object, got {}",
            field.name(),
            kind(value)
        ))
    })?;
    let key_field = entry
        .field
        .iter()
        .find(|f| f.number() == 1)
        .ok_or_else(|| {
            McpError::Descriptor(format!("map entry for {} has no key", field.name()))
        })?;
    let value_field = entry
        .field
        .iter()
        .find(|f| f.number() == 2)
        .ok_or_else(|| {
            McpError::Descriptor(format!("map entry for {} has no value", field.name()))
        })?;

    let tag = u64::from(field.number().unsigned_abs());
    for (key, item) in object {
        let mut body = Vec::new();
        // proto3 JSON always spells a map key as a string, whatever the
        // declared key type, so it is re-parsed here rather than required to
        // arrive as a JSON number.
        let key_value = if key_field.r#type() == Type::String {
            Value::String(key.clone())
        } else {
            serde_json::from_str::<Value>(key).map_err(|_| {
                McpError::InvalidArguments(format!(
                    "map key {key:?} is not a valid {:?} key",
                    key_field.r#type()
                ))
            })?
        };
        encode_single(catalog, key_field, &key_value, 1, depth + 1, &mut body)?;
        encode_single(catalog, value_field, item, 2, depth + 1, &mut body)?;
        put_key(tag, Wire::LengthDelimited, out);
        put_varint(body.len() as u64, out);
        out.extend_from_slice(&body);
    }
    Ok(())
}

/// One occurrence of one field, key included.
fn encode_single(
    catalog: &Catalog,
    field: &FieldDescriptorProto,
    value: &Value,
    tag: u64,
    depth: usize,
    out: &mut Vec<u8>,
) -> Result<(), McpError> {
    let ty = field.r#type();
    match ty {
        Type::Double => {
            put_key(tag, Wire::SixtyFourBit, out);
            out.extend_from_slice(&as_f64(field, value)?.to_le_bytes());
        }
        Type::Float => {
            put_key(tag, Wire::ThirtyTwoBit, out);
            #[allow(clippy::cast_possible_truncation)]
            out.extend_from_slice(&(as_f64(field, value)? as f32).to_le_bytes());
        }
        Type::Int64 | Type::Int32 => {
            let v = as_i64(field, value)?;
            if ty == Type::Int32 {
                range_i32(field, v)?;
            }
            put_key(tag, Wire::Varint, out);
            // Negative int32/int64 are both sign-extended to 64 bits, which is
            // why a negative int32 costs ten bytes on the wire.
            put_varint(v as u64, out);
        }
        Type::Sint64 | Type::Sint32 => {
            let v = as_i64(field, value)?;
            if ty == Type::Sint32 {
                range_i32(field, v)?;
            }
            put_key(tag, Wire::Varint, out);
            put_varint(((v << 1) ^ (v >> 63)) as u64, out);
        }
        Type::Uint64 | Type::Uint32 => {
            let v = as_u64(field, value)?;
            if ty == Type::Uint32 && v > u64::from(u32::MAX) {
                return Err(McpError::InvalidArguments(format!(
                    "{} is uint32 and {v} does not fit",
                    field.name()
                )));
            }
            put_key(tag, Wire::Varint, out);
            put_varint(v, out);
        }
        Type::Fixed64 => {
            put_key(tag, Wire::SixtyFourBit, out);
            out.extend_from_slice(&as_u64(field, value)?.to_le_bytes());
        }
        Type::Sfixed64 => {
            put_key(tag, Wire::SixtyFourBit, out);
            out.extend_from_slice(&as_i64(field, value)?.to_le_bytes());
        }
        Type::Fixed32 => {
            let v = as_u64(field, value)?;
            let v = u32::try_from(v).map_err(|_| {
                McpError::InvalidArguments(format!(
                    "{} is fixed32 and {v} does not fit",
                    field.name()
                ))
            })?;
            put_key(tag, Wire::ThirtyTwoBit, out);
            out.extend_from_slice(&v.to_le_bytes());
        }
        Type::Sfixed32 => {
            let v = as_i64(field, value)?;
            range_i32(field, v)?;
            put_key(tag, Wire::ThirtyTwoBit, out);
            #[allow(clippy::cast_possible_truncation)]
            out.extend_from_slice(&(v as i32).to_le_bytes());
        }
        Type::Bool => {
            let v = value.as_bool().ok_or_else(|| {
                McpError::InvalidArguments(format!(
                    "{} is a boolean, got {}",
                    field.name(),
                    kind(value)
                ))
            })?;
            put_key(tag, Wire::Varint, out);
            put_varint(u64::from(v), out);
        }
        Type::String => {
            let v = value.as_str().ok_or_else(|| {
                McpError::InvalidArguments(format!(
                    "{} is a string, got {}",
                    field.name(),
                    kind(value)
                ))
            })?;
            put_key(tag, Wire::LengthDelimited, out);
            put_varint(v.len() as u64, out);
            out.extend_from_slice(v.as_bytes());
        }
        Type::Bytes => {
            let encoded = value.as_str().ok_or_else(|| {
                McpError::InvalidArguments(format!(
                    "{} is bytes and must be a base64 string, got {}",
                    field.name(),
                    kind(value)
                ))
            })?;
            let raw = BASE64.decode(encoded).map_err(|e| {
                McpError::InvalidArguments(format!("{} is not valid base64: {e}", field.name()))
            })?;
            put_key(tag, Wire::LengthDelimited, out);
            put_varint(raw.len() as u64, out);
            out.extend_from_slice(&raw);
        }
        Type::Enum => {
            let number = enum_number(catalog, field, value)?;
            put_key(tag, Wire::Varint, out);
            put_varint(i64::from(number) as u64, out);
        }
        Type::Message => {
            let mut body = Vec::new();
            encode_message(catalog, field.type_name(), value, depth + 1, &mut body)?;
            put_key(tag, Wire::LengthDelimited, out);
            put_varint(body.len() as u64, out);
            out.extend_from_slice(&body);
        }
        Type::Group => {
            return Err(McpError::Wire(format!(
                "{} is a protobuf group, which is not supported",
                field.name()
            )))
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Decoding: protobuf -> JSON
// ---------------------------------------------------------------------------

/// Decode the protobuf wire form of `message_name` into JSON.
///
/// Only fields actually present on the wire appear in the result. That is
/// proto3's own JSON convention and it is the right one for a tool result: a
/// message padded out with every zero-valued field reads, to a model, as a
/// statement that those values were measured.
///
/// # Errors
///
/// [`McpError::Wire`] if `bytes` is not a well-formed message;
/// [`McpError::Descriptor`] if a referenced type is missing.
pub fn decode(catalog: &Catalog, message_name: &str, bytes: &[u8]) -> Result<Value, McpError> {
    let mut cursor = bytes;
    let value = decode_message(catalog, message_name, &mut cursor, 0)?;
    if !cursor.is_empty() {
        return Err(McpError::Wire(format!(
            "{} trailing byte(s) after {message_name}",
            cursor.len()
        )));
    }
    Ok(value)
}

fn decode_message(
    catalog: &Catalog,
    message_name: &str,
    buf: &mut &[u8],
    depth: usize,
) -> Result<Value, McpError> {
    if depth > MAX_NESTING {
        return Err(McpError::Wire(format!(
            "response nests deeper than {MAX_NESTING} messages"
        )));
    }
    let name = message_name.trim_start_matches('.');
    let message = catalog.message(name).ok_or_else(|| {
        McpError::Descriptor(format!("no message named {name} in the descriptor set"))
    })?;
    let by_number: HashMap<i32, &FieldDescriptorProto> =
        message.field.iter().map(|f| (f.number(), f)).collect();

    let mut out = Map::new();
    while !buf.is_empty() {
        let key = take_varint(buf)?;
        let tag = i32::try_from(key >> 3)
            .map_err(|_| McpError::Wire(format!("field number {} is out of range", key >> 3)))?;
        let wire = Wire::from_bits(key & 7)?;

        let Some(field) = by_number.get(&tag).copied() else {
            // An unknown field is a newer peer, not a corrupt message. The
            // daemon and this projection are the same binary today, but the
            // decoder has no business being the thing that breaks when they
            // are not.
            skip(wire, buf)?;
            continue;
        };
        decode_field(catalog, field, wire, buf, depth, &mut out)?;
    }
    Ok(Value::Object(out))
}

fn decode_field(
    catalog: &Catalog,
    field: &FieldDescriptorProto,
    wire: Wire,
    buf: &mut &[u8],
    depth: usize,
    out: &mut Map<String, Value>,
) -> Result<(), McpError> {
    let key = field.name().to_owned();
    let repeated = field.label() == Label::Repeated;

    if repeated {
        if let Some(entry) = map_entry(catalog, field) {
            // Checked here rather than left to `take_slice`'s bound: a map
            // entry arriving with any other wire type would otherwise have its
            // *value* read as a length prefix and silently mis-parse into a
            // plausible-looking map, where every other field's wire type is
            // validated in `decode_single`.
            if wire != Wire::LengthDelimited {
                return Err(McpError::Wire(format!(
                    "{} is a map and must arrive length-delimited, got {wire:?}",
                    field.name()
                )));
            }
            let len = usize::try_from(take_varint(buf)?)
                .map_err(|_| McpError::Wire("map entry length out of range".to_owned()))?;
            let mut body = take_slice(buf, len)?;
            decode_map_entry(catalog, entry, &mut body, depth, out, &key)?;
            return Ok(());
        }
        // A packable scalar written packed arrives as one length-delimited run
        // holding every element; the same field written unpacked arrives as
        // repeated single keys. Both are legal for the same declaration, so
        // both are read.
        let packable = !matches!(
            field.r#type(),
            Type::String | Type::Bytes | Type::Message | Type::Group
        );
        if packable && wire == Wire::LengthDelimited {
            let len = usize::try_from(take_varint(buf)?)
                .map_err(|_| McpError::Wire("packed run length out of range".to_owned()))?;
            let mut run = take_slice(buf, len)?;
            while !run.is_empty() {
                let element =
                    decode_single(catalog, field, wire_for(field.r#type()), &mut run, depth)?;
                push(out, &key, element);
            }
            return Ok(());
        }
        let element = decode_single(catalog, field, wire, buf, depth)?;
        push(out, &key, element);
        return Ok(());
    }

    let value = decode_single(catalog, field, wire, buf, depth)?;
    out.insert(key, value);
    Ok(())
}

fn decode_map_entry(
    catalog: &Catalog,
    entry: &DescriptorProto,
    body: &mut &[u8],
    depth: usize,
    out: &mut Map<String, Value>,
    key: &str,
) -> Result<(), McpError> {
    let mut map_key: Option<Value> = None;
    let mut map_value: Option<Value> = None;
    while !body.is_empty() {
        let raw = take_varint(body)?;
        let number = i32::try_from(raw >> 3)
            .map_err(|_| McpError::Wire("map entry field number out of range".to_owned()))?;
        let wire = Wire::from_bits(raw & 7)?;
        match entry.field.iter().find(|f| f.number() == number) {
            Some(f) if number == 1 => {
                map_key = Some(decode_single(catalog, f, wire, body, depth + 1)?);
            }
            Some(f) if number == 2 => {
                map_value = Some(decode_single(catalog, f, wire, body, depth + 1)?);
            }
            _ => skip(wire, body)?,
        }
    }
    // An entry may legally omit either half; proto3 fills it with the type's
    // default, and for JSON that means an empty key or a null value rather
    // than a dropped pair.
    let key_text = match map_key {
        Some(Value::String(s)) => s,
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let slot = out
        .entry(key.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(object) = slot.as_object_mut() {
        object.insert(key_text, map_value.unwrap_or(Value::Null));
    }
    Ok(())
}

fn decode_single(
    catalog: &Catalog,
    field: &FieldDescriptorProto,
    wire: Wire,
    buf: &mut &[u8],
    depth: usize,
) -> Result<Value, McpError> {
    let expected = wire_for(field.r#type());
    if wire != expected {
        return Err(McpError::Wire(format!(
            "{} is {:?} but arrived with wire type {wire:?}",
            field.name(),
            field.r#type()
        )));
    }
    Ok(match field.r#type() {
        Type::Double => number(f64::from_le_bytes(take_array::<8>(buf)?)),
        Type::Float => number(f64::from(f32::from_le_bytes(take_array::<4>(buf)?))),
        Type::Int64 => int64(take_varint(buf)? as i64),
        #[allow(clippy::cast_possible_truncation)]
        Type::Int32 => Value::Number(Number::from(take_varint(buf)? as i64 as i32)),
        Type::Uint64 => uint64(take_varint(buf)?),
        Type::Uint32 => Value::Number(Number::from(
            u32::try_from(take_varint(buf)? & 0xffff_ffff).unwrap_or(u32::MAX),
        )),
        Type::Sint64 => int64(unzigzag(take_varint(buf)?)),
        #[allow(clippy::cast_possible_truncation)]
        Type::Sint32 => Value::Number(Number::from(unzigzag(take_varint(buf)?) as i32)),
        Type::Fixed64 => uint64(u64::from_le_bytes(take_array::<8>(buf)?)),
        Type::Sfixed64 => int64(i64::from_le_bytes(take_array::<8>(buf)?)),
        Type::Fixed32 => Value::Number(Number::from(u32::from_le_bytes(take_array::<4>(buf)?))),
        Type::Sfixed32 => Value::Number(Number::from(i32::from_le_bytes(take_array::<4>(buf)?))),
        Type::Bool => Value::Bool(take_varint(buf)? != 0),
        Type::Enum => {
            #[allow(clippy::cast_possible_truncation)]
            let number = take_varint(buf)? as i64 as i32;
            let name = field.type_name().trim_start_matches('.');
            match catalog
                .enumeration(name)
                .and_then(|e| e.value.iter().find(|v| v.number() == number))
            {
                Some(value) => Value::String(value.name().to_owned()),
                // An enum value this build does not know is reported as the
                // number rather than dropped: the caller can still see what
                // arrived, which is the whole point of proto3's open enums.
                None => Value::Number(Number::from(number)),
            }
        }
        Type::String => {
            let len = usize::try_from(take_varint(buf)?)
                .map_err(|_| McpError::Wire("string length out of range".to_owned()))?;
            let raw = take_slice(buf, len)?;
            Value::String(
                std::str::from_utf8(raw)
                    .map_err(|e| {
                        McpError::Wire(format!("{} is not valid UTF-8: {e}", field.name()))
                    })?
                    .to_owned(),
            )
        }
        Type::Bytes => {
            let len = usize::try_from(take_varint(buf)?)
                .map_err(|_| McpError::Wire("bytes length out of range".to_owned()))?;
            Value::String(BASE64.encode(take_slice(buf, len)?))
        }
        Type::Message => {
            let len = usize::try_from(take_varint(buf)?)
                .map_err(|_| McpError::Wire("message length out of range".to_owned()))?;
            let mut body = take_slice(buf, len)?;
            let value = decode_message(catalog, field.type_name(), &mut body, depth + 1)?;
            if !body.is_empty() {
                return Err(McpError::Wire(format!(
                    "{} had {} trailing byte(s)",
                    field.name(),
                    body.len()
                )));
            }
            value
        }
        Type::Group => {
            return Err(McpError::Wire(format!(
                "{} is a protobuf group, which is not supported",
                field.name()
            )))
        }
    })
}

// ---------------------------------------------------------------------------
// Scalar conversions
// ---------------------------------------------------------------------------

/// A 64-bit integer as JSON.
///
/// Emitted as a number while it round-trips exactly through an IEEE-754
/// double, and as a string past that. Canonical proto3 JSON always uses a
/// string, but a message id rendered as `"41"` is one a model tends to quote
/// back with the quotes, and every id in this workspace is far inside the
/// exact range. Beyond it the string form is not a preference but a
/// correctness requirement: a JSON number would silently round.
fn int64(value: i64) -> Value {
    const EXACT: i64 = 1 << 53;
    if (-EXACT..=EXACT).contains(&value) {
        Value::Number(Number::from(value))
    } else {
        Value::String(value.to_string())
    }
}

fn uint64(value: u64) -> Value {
    const EXACT: u64 = 1 << 53;
    if value <= EXACT {
        Value::Number(Number::from(value))
    } else {
        Value::String(value.to_string())
    }
}

/// A float as JSON, with the non-finite values proto3 JSON spells as strings.
fn number(value: f64) -> Value {
    Number::from_f64(value).map_or_else(
        || {
            Value::String(if value.is_nan() {
                "NaN".to_owned()
            } else if value.is_sign_positive() {
                "Infinity".to_owned()
            } else {
                "-Infinity".to_owned()
            })
        },
        Value::Number,
    )
}

fn as_f64(field: &FieldDescriptorProto, value: &Value) -> Result<f64, McpError> {
    if let Some(v) = value.as_f64() {
        return Ok(v);
    }
    match value.as_str() {
        Some("NaN") => Ok(f64::NAN),
        Some("Infinity") => Ok(f64::INFINITY),
        Some("-Infinity") => Ok(f64::NEG_INFINITY),
        Some(text) => text.parse::<f64>().map_err(|_| {
            McpError::InvalidArguments(format!(
                "{} is a number, got the string {text:?}",
                field.name()
            ))
        }),
        None => Err(McpError::InvalidArguments(format!(
            "{} is a number, got {}",
            field.name(),
            kind(value)
        ))),
    }
}

fn as_i64(field: &FieldDescriptorProto, value: &Value) -> Result<i64, McpError> {
    if let Some(v) = value.as_i64() {
        return Ok(v);
    }
    // A JSON string holding an integer is the canonical proto3 JSON form for
    // 64-bit fields, so it is accepted alongside the number the generated
    // schema advertises.
    if let Some(text) = value.as_str() {
        return text.trim().parse::<i64>().map_err(|_| {
            McpError::InvalidArguments(format!(
                "{} is an integer, got the string {text:?}",
                field.name()
            ))
        });
    }
    Err(McpError::InvalidArguments(format!(
        "{} is an integer, got {}",
        field.name(),
        kind(value)
    )))
}

fn as_u64(field: &FieldDescriptorProto, value: &Value) -> Result<u64, McpError> {
    if let Some(v) = value.as_u64() {
        return Ok(v);
    }
    if let Some(text) = value.as_str() {
        return text.trim().parse::<u64>().map_err(|_| {
            McpError::InvalidArguments(format!(
                "{} is an unsigned integer, got the string {text:?}",
                field.name()
            ))
        });
    }
    Err(McpError::InvalidArguments(format!(
        "{} is an unsigned integer, got {}",
        field.name(),
        kind(value)
    )))
}

fn range_i32(field: &FieldDescriptorProto, value: i64) -> Result<(), McpError> {
    if i32::try_from(value).is_err() {
        return Err(McpError::InvalidArguments(format!(
            "{} is a 32-bit integer and {value} does not fit",
            field.name()
        )));
    }
    Ok(())
}

/// An enum value from its name (preferred) or its number.
fn enum_number(
    catalog: &Catalog,
    field: &FieldDescriptorProto,
    value: &Value,
) -> Result<i32, McpError> {
    let name = field.type_name().trim_start_matches('.');
    let enumeration = catalog.enumeration(name).ok_or_else(|| {
        McpError::Descriptor(format!("no enum named {name} in the descriptor set"))
    })?;
    if let Some(text) = value.as_str() {
        return enumeration
            .value
            .iter()
            .find(|v| v.name() == text)
            .map(prost_types::EnumValueDescriptorProto::number)
            .ok_or_else(|| {
                let names: Vec<&str> = enumeration.value.iter().map(|v| v.name()).collect();
                McpError::InvalidArguments(format!(
                    "{} has no value {text:?}; expected one of {}",
                    field.name(),
                    names.join(", ")
                ))
            });
    }
    let number = as_i64(field, value)?;
    i32::try_from(number).map_err(|_| {
        McpError::InvalidArguments(format!(
            "{} is an enum and {number} is not a valid value number",
            field.name()
        ))
    })
}

/// The JSON type name, for error messages a model has to act on.
fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn push(out: &mut Map<String, Value>, key: &str, value: Value) {
    match out.get_mut(key) {
        Some(Value::Array(items)) => items.push(value),
        _ => {
            out.insert(key.to_owned(), Value::Array(vec![value]));
        }
    }
}

// ---------------------------------------------------------------------------
// Wire primitives
// ---------------------------------------------------------------------------

fn put_key(tag: u64, wire: Wire, out: &mut Vec<u8>) {
    put_varint((tag << 3) | wire.bits(), out);
}

fn put_varint(mut value: u64, out: &mut Vec<u8>) {
    loop {
        #[allow(clippy::cast_possible_truncation)]
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn take_varint(buf: &mut &[u8]) -> Result<u64, McpError> {
    let mut result = 0u64;
    for shift in 0..10 {
        let byte = *buf
            .first()
            .ok_or_else(|| McpError::Wire("message ended mid-varint".to_owned()))?;
        *buf = &buf[1..];
        result |= u64::from(byte & 0x7f) << (shift * 7);
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
    Err(McpError::Wire("varint longer than 10 bytes".to_owned()))
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

fn take_slice<'a>(buf: &mut &'a [u8], len: usize) -> Result<&'a [u8], McpError> {
    if buf.len() < len {
        return Err(McpError::Wire(format!(
            "message claims {len} byte(s) but only {} remain",
            buf.len()
        )));
    }
    let (head, tail) = buf.split_at(len);
    *buf = tail;
    Ok(head)
}

fn take_array<const N: usize>(buf: &mut &[u8]) -> Result<[u8; N], McpError> {
    let slice = take_slice(buf, N)?;
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    Ok(out)
}

fn skip(wire: Wire, buf: &mut &[u8]) -> Result<(), McpError> {
    match wire {
        Wire::Varint => {
            take_varint(buf)?;
        }
        Wire::SixtyFourBit => {
            take_slice(buf, 8)?;
        }
        Wire::ThirtyTwoBit => {
            take_slice(buf, 4)?;
        }
        Wire::LengthDelimited => {
            let len = usize::try_from(take_varint(buf)?)
                .map_err(|_| McpError::Wire("length prefix out of range".to_owned()))?;
            take_slice(buf, len)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
