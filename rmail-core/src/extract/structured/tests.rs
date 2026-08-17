//! Schema admission, validation, and the storage round trip.
//!
//! The validator is the security-relevant half of this module: it is what
//! makes "validated and stored" true of a row rather than aspirational. So the
//! tests here are mostly *rejections* — a document that should not have been
//! stored, and a schema that should not have been accepted. A validator that
//! only proves conforming documents pass proves nothing at all.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

use super::*;
use crate::extract::store;
use crate::storage::Database;
use crate::{repo, ErrorReason};

// ---------------------------------------------------------------------------
// The built-ins
// ---------------------------------------------------------------------------

#[test]
fn every_builtin_has_a_schema_this_build_can_enforce() {
    for schema in BUILTINS {
        let value = schema_for(schema.name).expect("a built-in name always has a schema");
        assert!(
            check_schema(&value).is_ok(),
            "{}: {:?}",
            schema.name,
            check_schema(&value)
        );
        assert_eq!(builtin(schema.name).map(|b| b.name), Some(schema.name));
        assert!(!schema.instruction.is_empty());
        assert!(!schema.summary.is_empty());
    }
}

#[test]
fn an_unknown_schema_name_has_no_schema() {
    assert_eq!(schema_for("horoscope"), None);
    assert_eq!(builtin("horoscope"), None);
}

#[test]
fn a_builtins_own_shape_validates_against_itself() {
    let schema = schema_for("flight").expect("flight");
    let document = json!({
        "carrier": "BA", "flight_number": "BA117", "booking_reference": "X7Q2LM",
        "passenger": "Ada Lovelace", "origin": "LHR", "destination": "JFK",
        "departs_at": "2024-07-01T08:40+01:00", "arrives_at": "2024-07-01T11:35-04:00",
        "seat": "14A",
    });
    validate(&schema, &document).expect("a conforming document");
}

#[test]
fn schema_hashes_are_stable_and_distinguish_shapes() {
    let a = schema_for("flight").expect("flight");
    let b = schema_for("flight").expect("flight");
    let c = schema_for("meeting").expect("meeting");
    assert_eq!(schema_hash(&a), schema_hash(&b));
    assert_ne!(schema_hash(&a), schema_hash(&c));
    assert_eq!(schema_hash(&a).len(), 64);
}

// ---------------------------------------------------------------------------
// Validation: the rejections
// ---------------------------------------------------------------------------

fn tiny() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "count": {"type": "integer"},
        },
        "required": ["name"],
        "additionalProperties": false,
    })
}

#[test]
fn a_missing_required_field_is_named() {
    let error = validate(&tiny(), &json!({"count": 1})).expect_err("required");
    assert_eq!(error.reason(), ErrorReason::Internal);
    assert!(error.to_string().contains("\"name\""), "{error}");
}

#[test]
fn a_wrong_type_is_named_with_what_it_actually_was() {
    let error = validate(&tiny(), &json!({"name": 7})).expect_err("type");
    assert!(error.to_string().contains("should be a string"), "{error}");
    assert!(error.to_string().contains("is number"), "{error}");
}

#[test]
fn an_undeclared_field_is_rejected_when_additional_properties_is_false() {
    let error =
        validate(&tiny(), &json!({"name": "a", "extra": 1})).expect_err("additionalProperties");
    assert!(error.to_string().contains("\"extra\""), "{error}");
}

#[test]
fn an_undeclared_field_is_allowed_when_the_schema_permits_it() {
    let schema = json!({
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "required": ["name"],
        "additionalProperties": true,
    });
    validate(&schema, &json!({"name": "a", "extra": 1})).expect("permitted");
}

#[test]
fn a_float_is_not_an_integer() {
    // JSON Schema admits 3.0 as an integer; this validator does not, because
    // the consumer of an integer field is arithmetic.
    validate(&tiny(), &json!({"name": "a", "count": 3})).expect("an integer");
    let error = validate(&tiny(), &json!({"name": "a", "count": 3.5})).expect_err("a float");
    assert!(error.to_string().contains("count"), "{error}");
}

#[test]
fn enum_length_and_numeric_bounds_are_enforced() {
    let schema = json!({
        "type": "object",
        "properties": {
            "colour": {"type": "string", "enum": ["red", "green"]},
            "score": {"type": "number", "minimum": 0.0, "maximum": 1.0},
            "code": {"type": "string", "minLength": 2, "maxLength": 3},
        },
        "required": [],
        "additionalProperties": false,
    });
    check_schema(&schema).expect("admissible");
    validate(
        &schema,
        &json!({"colour": "red", "score": 0.5, "code": "abc"}),
    )
    .expect("valid");

    for (bad, needle) in [
        (json!({"colour": "blue"}), "allows"),
        (json!({"score": 1.5}), "above the maximum"),
        (json!({"score": -0.5}), "below the minimum"),
        (json!({"code": "a"}), "shorter than"),
        (json!({"code": "abcd"}), "longer than"),
    ] {
        let error = validate(&schema, &bad).expect_err("should be rejected");
        assert!(error.to_string().contains(needle), "{bad}: {error}");
    }
}

#[test]
fn a_nested_violation_names_its_json_pointer() {
    let schema = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"n": {"type": "integer"}},
                    "required": ["n"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["items"],
        "additionalProperties": false,
    });
    let error = validate(&schema, &json!({"items": [{"n": 1}, {"n": "two"}]}))
        .expect_err("the second item");
    assert!(error.to_string().contains("/items/1/n"), "{error}");
}

#[test]
fn an_undeclared_field_of_any_depth_is_accepted_without_being_walked() {
    // The invariant the module docs rest on: the walk descends only where the
    // schema declares a child, so recursion is bounded by the schema and not
    // by the answer. A thousand-deep undeclared value must cost one comparison
    // rather than a thousand stack frames.
    let schema = json!({"type": "object", "properties": {}, "additionalProperties": true});
    let mut value = json!(1);
    for _ in 0..1_000 {
        value = json!([value]);
    }
    validate(&schema, &json!({"deep": value})).expect("accepted without recursing");
}

#[test]
fn a_declared_child_is_walked_only_as_deep_as_the_schema_goes() {
    // The mirror of the test above: where the schema *does* declare a child,
    // the value is entered — and the depth it can be entered to is exactly the
    // schema's own, which `check_schema` caps at MAX_SCHEMA_DEPTH.
    let schema = json!({
        "type": "object",
        "properties": {"rows": {"type": "array", "items": {"type": "string"}}},
        "required": ["rows"],
        "additionalProperties": false,
    });
    check_schema(&schema).expect("admissible");
    validate(&schema, &json!({"rows": ["a", "b"]})).expect("valid");
    // One level deeper than the schema declares: the array's items are typed
    // `string`, so a nested array is caught rather than descended into.
    let error = validate(&schema, &json!({"rows": [["a"]]})).expect_err("nested");
    assert!(error.to_string().contains("/rows/0"), "{error}");
}

// ---------------------------------------------------------------------------
// Schema admission: the rejections
// ---------------------------------------------------------------------------

#[test]
fn a_root_that_is_not_an_object_is_refused() {
    let error = check_schema(&json!({"type": "array", "items": {"type": "string"}}))
        .expect_err("array root");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(error.to_string().contains("root"), "{error}");
}

#[test]
fn a_keyword_this_build_cannot_enforce_is_refused_rather_than_ignored() {
    // Ignoring it would make the schema *look* like it constrains and not, and
    // the document would be stored as "validated".
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "string", "pattern": "^x"}},
        "additionalProperties": false,
    });
    let error = check_schema(&schema).expect_err("pattern");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(error.to_string().contains("\"pattern\""), "{error}");
}

#[test]
fn a_schema_past_the_size_bound_is_refused() {
    let mut properties = serde_json::Map::new();
    for n in 0..2_000 {
        properties.insert(
            format!("field_with_a_long_name_{n}"),
            json!({"type": "string"}),
        );
    }
    let schema = json!({
        "type": "object",
        "properties": Value::Object(properties),
        "additionalProperties": false,
    });
    assert!(schema.to_string().len() > MAX_SCHEMA_BYTES);
    let error = check_schema(&schema).expect_err("too large");
    // Specific to the *size* bound: the property-count message also ends
    // "the limit is", so the looser assertion passed with the size check
    // deleted.
    assert!(error.to_string().contains("bytes; the limit is"), "{error}");
}

#[test]
fn a_schema_past_the_property_bound_is_refused() {
    let mut properties = serde_json::Map::new();
    for n in 0..(MAX_SCHEMA_PROPERTIES + 5) {
        properties.insert(format!("f{n}"), json!({"type": "string"}));
    }
    let schema = json!({
        "type": "object",
        "properties": Value::Object(properties),
        "additionalProperties": false,
    });
    assert!(schema.to_string().len() < MAX_SCHEMA_BYTES);
    let error = check_schema(&schema).expect_err("too wide");
    assert!(
        error.to_string().contains("properties on one object"),
        "{error}"
    );
}

#[test]
fn a_schema_past_the_depth_bound_is_refused() {
    let mut node = json!({"type": "string"});
    for _ in 0..(MAX_SCHEMA_DEPTH + 3) {
        node = json!({"type": "array", "items": node});
    }
    let schema = json!({
        "type": "object",
        "properties": {"deep": node},
        "additionalProperties": false,
    });
    let error = check_schema(&schema).expect_err("too deep");
    assert!(error.to_string().contains("nests deeper"), "{error}");
}

#[test]
fn an_empty_or_enormous_enum_is_refused() {
    let with = |variants: Vec<Value>| {
        json!({
            "type": "object",
            "properties": {"a": {"type": "string", "enum": Value::Array(variants)}},
            "additionalProperties": false,
        })
    };
    assert!(check_schema(&with(Vec::new())).is_err());
    let many: Vec<Value> = (0..(MAX_ENUM_VARIANTS + 1))
        .map(|n| Value::String(n.to_string()))
        .collect();
    assert!(check_schema(&with(many)).is_err());
    assert!(check_schema(&with(vec![json!("only")])).is_ok());
}

// ---------------------------------------------------------------------------
// Model answers
// ---------------------------------------------------------------------------

#[test]
fn an_answer_that_is_not_json_is_internal_not_invalid_argument() {
    // The caller cannot fix a model's output and must not be told they wrote
    // something wrong.
    let error = from_model_answer(&tiny(), "not json at all").expect_err("not JSON");
    assert_eq!(error.reason(), ErrorReason::Internal);
}

#[test]
fn an_answer_past_the_size_bound_is_refused_before_it_is_parsed() {
    let huge = format!("{{\"name\": \"{}\"}}", "x".repeat(MAX_ANSWER_BYTES));
    let error = from_model_answer(&tiny(), &huge).expect_err("too large");
    assert!(error.to_string().contains("the limit is"), "{error}");
}

#[test]
fn an_answer_the_provider_should_have_constrained_is_still_checked_here() {
    // The whole point of re-validating: a provider that returned the wrong
    // shape must not get a row.
    let error = from_model_answer(&tiny(), "{\"count\": 1}").expect_err("missing name");
    assert!(error.to_string().contains("\"name\""), "{error}");
}

#[test]
fn a_conforming_answer_comes_back_as_the_document_to_store() {
    let value = from_model_answer(&tiny(), "{\"name\": \"Ada\", \"count\": 2}").expect("valid");
    assert_eq!(value, json!({"name": "Ada", "count": 2}));
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    db: Database,
    message_id: i64,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-extract-structured-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open");
        let message_id = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid: 1,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )
            })
            .await
            .expect("seed");
        Self {
            db,
            message_id,
            path,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

#[tokio::test]
async fn a_stored_extraction_round_trips() {
    let fixture = Fixture::open().await;
    let schema = schema_for("meeting").expect("meeting");
    let hash = schema_hash(&schema);
    let data = json!({
        "title": "Design review", "organizer": "ada@example.com",
        "attendees": ["ada@example.com"], "starts_at": "2024-04-02T15:00Z",
        "ends_at": "", "location": "", "conference_url": "", "agenda": "",
    });
    validate(&schema, &data).expect("valid");

    let saved = store::save_extraction(
        &fixture.db,
        fixture.message_id,
        "meeting",
        &hash,
        "claude-haiku-4-5",
        &data,
    )
    .await
    .expect("save");
    assert!(saved.extraction_id > 0);
    assert_eq!(saved.model, "claude-haiku-4-5");

    let found = store::find_extraction(&fixture.db, fixture.message_id, "meeting", &hash)
        .await
        .expect("find")
        .expect("a row");
    assert_eq!(found.extraction_id, saved.extraction_id);
    assert_eq!(
        serde_json::from_str::<Value>(&found.data).expect("json"),
        data
    );
}

#[tokio::test]
async fn a_different_schema_hash_stores_a_second_document_rather_than_overwriting() {
    let fixture = Fixture::open().await;
    let data = json!({"name": "Ada"});
    store::save_extraction(&fixture.db, fixture.message_id, CUSTOM, "aaa", "m", &data)
        .await
        .expect("first");
    store::save_extraction(
        &fixture.db,
        fixture.message_id,
        CUSTOM,
        "bbb",
        "m",
        &json!({"name": "Grace"}),
    )
    .await
    .expect("second");

    assert!(
        store::find_extraction(&fixture.db, fixture.message_id, CUSTOM, "aaa")
            .await
            .expect("find")
            .is_some(),
        "the first reading must survive a differently-shaped second one"
    );
    let second = store::find_extraction(&fixture.db, fixture.message_id, CUSTOM, "bbb")
        .await
        .expect("find")
        .expect("second");
    assert!(second.data.contains("Grace"));
}

#[tokio::test]
async fn re_extracting_the_same_schema_replaces_the_row() {
    let fixture = Fixture::open().await;
    store::save_extraction(
        &fixture.db,
        fixture.message_id,
        CUSTOM,
        "aaa",
        "m",
        &json!({"name": "Ada"}),
    )
    .await
    .expect("first");
    let second = store::save_extraction(
        &fixture.db,
        fixture.message_id,
        CUSTOM,
        "aaa",
        "m",
        &json!({"name": "Grace"}),
    )
    .await
    .expect("second");

    let count: i64 = fixture
        .db
        .with_read(|c| {
            c.query_row("SELECT count(*) FROM structured_extractions", [], |r| {
                r.get(0)
            })
        })
        .expect("count");
    assert_eq!(count, 1);
    assert!(second.data.contains("Grace"));
}

#[tokio::test]
async fn a_message_with_no_extraction_finds_nothing() {
    let fixture = Fixture::open().await;
    assert!(
        store::find_extraction(&fixture.db, fixture.message_id, "invoice", "aaa")
            .await
            .expect("find")
            .is_none()
    );
}

#[test]
fn a_known_keyword_with_a_wrong_typed_value_is_refused_rather_than_ignored() {
    // Every one of these passes a *name* check and then reads back as `None`
    // through `check_value`'s lenient accessors — so the schema would appear
    // to constrain and would not. That is the failure the module docs rule
    // out, and it is worse than an unknown keyword because it looks correct.
    for (schema, why) in [
        (
            json!({"type": "object", "required": "name", "properties": {}}),
            "required as a bare string checks nothing",
        ),
        (
            json!({"type": "object", "required": [7], "properties": {}}),
            "required listing a non-string",
        ),
        (
            json!({
                "type": "object",
                "properties": {"a": {"type": "string"}},
                "additionalProperties": {"type": "string"},
            }),
            "additionalProperties as a schema allows anything",
        ),
        (
            json!({
                "type": "object",
                "properties": {"a": {"type": "string", "minLength": "2"}},
            }),
            "minLength as a string enforces nothing",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "number", "minimum": "0"}}}),
            "minimum as a string enforces nothing",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": 7}}}),
            "a non-string type checks nothing",
        ),
    ] {
        let error = check_schema(&schema).expect_err(why);
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{why}");
    }
}

#[test]
fn a_type_this_build_cannot_check_is_refused_before_any_spend() {
    // Admitted, this reaches the provider (and the ledger) and only then
    // returns INTERNAL — billing a caller for their own typo.
    let schema = json!({
        "type": "object",
        "properties": {"a": {"type": "strnig"}},
        "additionalProperties": false,
    });
    let error = check_schema(&schema).expect_err("typo'd type");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(error.to_string().contains("strnig"), "{error}");
}
