//! How Harness² keeps what it does: as records in the store, through the
//! port, so any store keeps them.

use protocol::{Actor, DataClass, Trust};
use serde::Serialize;
use serde_json::Value;
use store::{
    FieldKind, FilterableField, Provenance, Record, RecordDecls, RecordId, RecordTypeDecl,
};
use uuid::Uuid;

use crate::attempt::Attempt;
use crate::calibrate::Calibration;
use crate::change::{Change, Hold};

/// The record type of an attempt.
pub const ATTEMPT_TYPE: &str = "harness.attempt";
/// The record type of a change to an agent.
pub const CHANGE_TYPE: &str = "harness.change";
/// The record type of a hold on an agent.
pub const HOLD_TYPE: &str = "harness.hold";
/// The record type of a judge model's known-answer test.
pub const CALIBRATION_TYPE: &str = "harness.calibration";

/// The longest a filterable enum's name is.
const MAX_ENUM_CHARS: u64 = 16;

/// `T`'s schema, made filterable on `fields`. A store filters only on a field
/// whose schema states its type, and an enum with documented variants is a
/// `oneOf` that doesn't, so its type is stated alongside it.
fn schema_for<T: schemars::JsonSchema>(fields: &[FilterableField]) -> Value {
    let mut schema = schema::schema_of::<T>();
    for field in fields {
        let pointer = format!("/properties/{}", field.name);
        if let Some(Value::Object(property)) = schema.pointer_mut(&pointer)
            && !property.contains_key("type")
            && field.kind == FieldKind::Text
        {
            property.insert("type".to_owned(), Value::from("string"));
            property.insert("maxLength".to_owned(), Value::from(MAX_ENUM_CHARS));
        }
    }
    schema
}

fn field(name: &str, kind: FieldKind) -> FilterableField {
    FilterableField {
        name: name.to_owned(),
        kind,
    }
}

fn declared<T: schemars::JsonSchema>(
    name: &str,
    filterable: Vec<FilterableField>,
) -> RecordTypeDecl {
    RecordTypeDecl {
        name: name.to_owned(),
        schema: schema_for::<T>(&filterable),
        filterable,
    }
}

/// The record types Harness² keeps.
#[must_use]
pub fn declarations() -> RecordDecls {
    RecordDecls {
        types: vec![
            declared::<Attempt>(
                ATTEMPT_TYPE,
                vec![
                    field("status", FieldKind::Text),
                    field("started_at", FieldKind::Timestamp),
                ],
            ),
            declared::<Change>(
                CHANGE_TYPE,
                vec![
                    field("agent", FieldKind::Text),
                    field("status", FieldKind::Text),
                    field("at", FieldKind::Timestamp),
                ],
            ),
            declared::<Hold>(HOLD_TYPE, vec![field("agent", FieldKind::Text)]),
            declared::<Calibration>(
                CALIBRATION_TYPE,
                vec![
                    field("model", FieldKind::Text),
                    field("passed", FieldKind::Bool),
                    field("at", FieldKind::Timestamp),
                ],
            ),
        ],
        relations: Vec::new(),
    }
}

/// A hold's id: one per agent, so holding twice is one hold and releasing
/// removes it.
#[must_use]
pub fn hold_id(agent: &str) -> RecordId {
    let seed = format!("harness.hold|{agent}");
    RecordId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, seed.as_bytes()))
}

/// A value as a Harness² record. What it holds came from judges or from runs
/// of private work, so it's untrusted and private.
///
/// # Errors
///
/// The serializer's message, which a bounded value never produces.
pub fn record<T: Serialize>(
    record_type: &str,
    id: RecordId,
    value: &T,
    at: chrono::DateTime<chrono::Utc>,
) -> Result<Record, serde_json::Error> {
    Ok(Record {
        record_type: record_type.to_owned(),
        id,
        data: serde_json::to_value(value)?,
        provenance: Provenance {
            data_class: DataClass::Private,
            trust: Trust::Untrusted,
            stored_by: Actor::new("harness"),
            stored_at: at,
        },
    })
}

/// An attempt as a record.
///
/// # Errors
///
/// The serializer's message, which a bounded attempt never produces.
pub fn record_of(id: RecordId, attempt: &Attempt) -> Result<Record, serde_json::Error> {
    record(ATTEMPT_TYPE, id, attempt, attempt.finished_at)
}
