//! How attempts are kept: as records in the store, through the port, so any
//! store keeps them.

use protocol::{Actor, DataClass, Trust};
use serde_json::Value;
use store::{
    FieldKind, FilterableField, Provenance, Record, RecordDecls, RecordId, RecordTypeDecl,
};

use crate::attempt::Attempt;

/// The record type of an attempt.
pub const ATTEMPT_TYPE: &str = "harness.attempt";

/// The schema of an attempt record. A store filters only on a field whose
/// schema says its type, and an enum with documented variants is a `oneOf`
/// with none, so the type is stated alongside it.
fn attempt_schema() -> Value {
    let mut schema = schema::schema_of::<Attempt>();
    if let Some(Value::Object(status)) = schema.pointer_mut("/properties/status") {
        status.insert("type".to_owned(), Value::String("string".to_owned()));
        status.insert("maxLength".to_owned(), Value::from(16));
    }
    schema
}

/// The record types Harness² keeps.
#[must_use]
pub fn declarations() -> RecordDecls {
    RecordDecls {
        types: vec![RecordTypeDecl {
            name: ATTEMPT_TYPE.to_owned(),
            schema: attempt_schema(),
            filterable: vec![
                FilterableField {
                    name: "status".to_owned(),
                    kind: FieldKind::Text,
                },
                FilterableField {
                    name: "started_at".to_owned(),
                    kind: FieldKind::Timestamp,
                },
            ],
        }],
        relations: Vec::new(),
    }
}

/// An attempt as a record. What judges wrote is model output, so it's
/// untrusted, and it describes private runs, so it's private.
///
/// # Errors
///
/// The serializer's message, which a bounded attempt never produces.
pub fn record_of(id: RecordId, attempt: &Attempt) -> Result<Record, serde_json::Error> {
    Ok(Record {
        record_type: ATTEMPT_TYPE.to_owned(),
        id,
        data: serde_json::to_value(attempt)?,
        provenance: Provenance {
            data_class: DataClass::Private,
            trust: Trust::Untrusted,
            stored_by: Actor::new("harness"),
            stored_at: attempt.finished_at,
        },
    })
}
