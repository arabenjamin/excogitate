//! The declared record types. Every store holds one of these and calls it,
//! so what "valid" means doesn't differ from store to store.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use schema::{Validator, check_strict};
use serde_json::Value;

use crate::{
    FieldFilter, FieldKind, FieldOp, FieldValue, Record, RecordDecls, RecordTypeDecl, RelationDecl,
    StoreError,
};

#[derive(Debug, Clone)]
struct CompiledType {
    decl: RecordTypeDecl,
    validator: Arc<Validator>,
}

/// The record types and relations plugins have declared. Plugins declare
/// them on every start, so the catalog itself isn't stored.
#[derive(Debug, Clone, Default)]
pub struct RecordCatalog {
    types: BTreeMap<String, CompiledType>,
    relations: BTreeMap<String, RelationDecl>,
}

impl RecordCatalog {
    /// An empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds declarations, all of them or none. Declaring the same thing
    /// twice is fine.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if a declaration is malformed, its schema
    /// isn't strict, or it names something undeclared.
    /// [`StoreError::Conflict`] if a name is already declared differently.
    pub fn register(&mut self, decls: &RecordDecls) -> Result<(), StoreError> {
        let raw =
            serde_json::to_value(decls).map_err(|error| StoreError::Invalid(error.to_string()))?;
        Validator::for_type::<RecordDecls>()?.check(&raw)?;

        let mut next = self.clone();
        for decl in &decls.types {
            next.add_type(decl)?;
        }
        for relation in &decls.relations {
            next.add_relation(relation)?;
        }
        *self = next;
        Ok(())
    }

    fn add_type(&mut self, decl: &RecordTypeDecl) -> Result<(), StoreError> {
        if let Some(existing) = self.types.get(&decl.name) {
            if existing.decl == *decl {
                return Ok(());
            }
            return Err(StoreError::Conflict(format!(
                "record type {} is already declared differently",
                decl.name
            )));
        }
        if let Some(violation) = check_strict(&decl.schema).first() {
            return Err(StoreError::Invalid(format!(
                "record type {}: schema isn't strict at {violation}",
                decl.name
            )));
        }
        for field in &decl.filterable {
            check_filterable(decl, &field.name, field.kind)?;
        }
        let validator = Arc::new(Validator::from_schema(&decl.schema)?);
        self.types.insert(
            decl.name.clone(),
            CompiledType {
                decl: decl.clone(),
                validator,
            },
        );
        Ok(())
    }

    fn add_relation(&mut self, relation: &RelationDecl) -> Result<(), StoreError> {
        if let Some(existing) = self.relations.get(&relation.name) {
            if existing == relation {
                return Ok(());
            }
            return Err(StoreError::Conflict(format!(
                "relation {} is already declared differently",
                relation.name
            )));
        }
        for end in [&relation.from, &relation.to] {
            if !self.types.contains_key(end) {
                return Err(StoreError::Invalid(format!(
                    "relation {} names undeclared type {end}",
                    relation.name
                )));
            }
        }
        self.relations
            .insert(relation.name.clone(), relation.clone());
        Ok(())
    }

    fn compiled(&self, record_type: &str) -> Result<&CompiledType, StoreError> {
        self.types
            .get(record_type)
            .ok_or_else(|| StoreError::Invalid(format!("undeclared record type {record_type}")))
    }

    /// Checks a record against its type's schema.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if the type is undeclared or the data fails.
    pub fn check_record(&self, record: &Record) -> Result<(), StoreError> {
        self.compiled(&record.record_type)?
            .validator
            .check(&record.data)?;
        Ok(())
    }

    /// Checks that every filter names a declared field, with a value of the
    /// field's type and a comparison that type supports.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] naming the first filter that fails.
    pub fn check_filters(
        &self,
        record_type: &str,
        filters: &[FieldFilter],
    ) -> Result<(), StoreError> {
        let decl = &self.compiled(record_type)?.decl;
        for filter in filters {
            let kind = field_kind(decl, &filter.field).ok_or_else(|| {
                StoreError::Invalid(format!(
                    "field {} of {record_type} isn't declared filterable",
                    filter.field
                ))
            })?;
            if !value_has_kind(&filter.value, kind) {
                return Err(StoreError::Invalid(format!(
                    "field {} is {kind:?}, and the filter's value isn't",
                    filter.field
                )));
            }
            if kind == FieldKind::Bool && filter.op != FieldOp::Eq {
                return Err(StoreError::Invalid(format!(
                    "field {} is a bool, which only supports equality",
                    filter.field
                )));
            }
        }
        Ok(())
    }

    /// The relation with this name.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if it isn't declared.
    pub fn relation(&self, name: &str) -> Result<&RelationDecl, StoreError> {
        self.relations
            .get(name)
            .ok_or_else(|| StoreError::Invalid(format!("undeclared relation {name}")))
    }

    /// The declared filterable fields a record has, each read as its
    /// declared kind. For stores that index filter fields themselves; a
    /// field that's missing or of another type is left out, so it matches
    /// no filter, as in [`matches`](Self::matches).
    #[must_use]
    pub fn filter_values<'a>(&'a self, record: &Record) -> Vec<(&'a str, FieldValue)> {
        let Some(compiled) = self.types.get(&record.record_type) else {
            return Vec::new();
        };
        compiled
            .decl
            .filterable
            .iter()
            .filter_map(|field| {
                read_field(&record.data, &field.name, field.kind)
                    .map(|value| (field.name.as_str(), value))
            })
            .collect()
    }

    /// Whether a record meets filters that have passed
    /// [`check_filters`](Self::check_filters). For stores that evaluate
    /// filters in process; a record lacking a field doesn't match.
    #[must_use]
    pub fn matches(&self, record: &Record, filters: &[FieldFilter]) -> bool {
        let Some(compiled) = self.types.get(&record.record_type) else {
            return false;
        };
        filters.iter().all(|filter| {
            field_kind(&compiled.decl, &filter.field)
                .and_then(|kind| read_field(&record.data, &filter.field, kind))
                .and_then(|value| compare(&value, &filter.value))
                .is_some_and(|ordering| op_accepts(filter.op, ordering))
        })
    }
}

fn check_filterable(decl: &RecordTypeDecl, field: &str, kind: FieldKind) -> Result<(), StoreError> {
    let property = decl.schema.get("properties").and_then(|p| p.get(field));
    let Some(property) = property else {
        return Err(StoreError::Invalid(format!(
            "record type {}: filterable field {field} isn't in the schema",
            decl.name
        )));
    };
    let expected = match kind {
        FieldKind::Text | FieldKind::Timestamp => "string",
        FieldKind::Integer => "integer",
        FieldKind::Number => "number",
        FieldKind::Bool => "boolean",
    };
    if property.get("type").and_then(Value::as_str) != Some(expected) {
        return Err(StoreError::Invalid(format!(
            "record type {}: field {field} must have type {expected} in the schema",
            decl.name
        )));
    }
    Ok(())
}

fn field_kind(decl: &RecordTypeDecl, field: &str) -> Option<FieldKind> {
    decl.filterable
        .iter()
        .find(|declared| declared.name == field)
        .map(|declared| declared.kind)
}

fn value_has_kind(value: &FieldValue, kind: FieldKind) -> bool {
    matches!(
        (value, kind),
        (FieldValue::Text(_), FieldKind::Text)
            | (FieldValue::Integer(_), FieldKind::Integer)
            | (FieldValue::Number(_), FieldKind::Number)
            | (FieldValue::Bool(_), FieldKind::Bool)
            | (FieldValue::Timestamp(_), FieldKind::Timestamp)
    )
}

fn read_field(data: &Value, field: &str, kind: FieldKind) -> Option<FieldValue> {
    let value = data.get(field)?;
    match kind {
        FieldKind::Text => value.as_str().map(|s| FieldValue::Text(s.to_owned())),
        FieldKind::Integer => value.as_i64().map(FieldValue::Integer),
        FieldKind::Number => value.as_f64().map(FieldValue::Number),
        FieldKind::Bool => value.as_bool().map(FieldValue::Bool),
        FieldKind::Timestamp => value
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|time| FieldValue::Timestamp(time.with_timezone(&Utc))),
    }
}

fn compare(left: &FieldValue, right: &FieldValue) -> Option<Ordering> {
    match (left, right) {
        (FieldValue::Text(a), FieldValue::Text(b)) => Some(a.cmp(b)),
        (FieldValue::Integer(a), FieldValue::Integer(b)) => Some(a.cmp(b)),
        (FieldValue::Number(a), FieldValue::Number(b)) => a.partial_cmp(b),
        (FieldValue::Bool(a), FieldValue::Bool(b)) => Some(a.cmp(b)),
        (FieldValue::Timestamp(a), FieldValue::Timestamp(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

fn op_accepts(op: FieldOp, ordering: Ordering) -> bool {
    match op {
        FieldOp::Eq => ordering.is_eq(),
        FieldOp::Lt => ordering.is_lt(),
        FieldOp::Lte => ordering.is_le(),
        FieldOp::Gt => ordering.is_gt(),
        FieldOp::Gte => ordering.is_ge(),
    }
}
