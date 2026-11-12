//! Records: the one generic kind, for plugin data. A plugin declares its
//! record types and relations with schemas, so a new plugin needs no change
//! to the core.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{PageRequest, Provenance, RecordId};

/// A plugin's declarations, as they appear in its manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordDecls {
    /// The record types.
    #[schemars(length(max = 64))]
    pub types: Vec<RecordTypeDecl>,
    /// The relations between them.
    #[schemars(length(max = 64))]
    pub relations: Vec<RelationDecl>,
}

/// One record type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordTypeDecl {
    /// Its name, as `plugin.type`.
    #[schemars(
        length(max = 128),
        regex(pattern = r"^[a-z][a-z0-9_]*\.[a-z][a-z0-9_]*$")
    )]
    pub name: String,
    /// The JSON Schema every record of this type must meet. It must pass the
    /// strictness check when it's registered.
    #[schemars(extend("x-extension" = "the plugin's own record schema, checked when registered"))]
    pub schema: Value,
    /// The top-level fields that can be filtered on.
    #[schemars(length(max = 32))]
    pub filterable: Vec<FilterableField>,
}

/// A field that can be filtered on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilterableField {
    /// The field's name.
    #[schemars(length(max = 128))]
    pub name: String,
    /// The field's type.
    pub kind: FieldKind,
}

/// The types a filterable field can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FieldKind {
    /// A string.
    Text,
    /// An integer.
    Integer,
    /// A number.
    Number,
    /// True or false.
    Bool,
    /// An RFC 3339 time, compared as a time, never as text.
    Timestamp,
}

/// A named link from records of one type to records of another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelationDecl {
    /// Its name, as `plugin.relation`.
    #[schemars(
        length(max = 128),
        regex(pattern = r"^[a-z][a-z0-9_]*\.[a-z][a-z0-9_]*$")
    )]
    pub name: String,
    /// The type of the records it links from.
    #[schemars(length(max = 128))]
    pub from: String,
    /// The type of the records it links to.
    #[schemars(length(max = 128))]
    pub to: String,
}

/// A plugin's stored item.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Record {
    /// Its type, as declared.
    pub record_type: String,
    /// Its id.
    pub id: RecordId,
    /// Its data, which must meet the type's schema.
    pub data: Value,
    /// Where it came from.
    pub provenance: Provenance,
}

impl Record {
    /// A record as a store read it.
    #[must_use]
    pub const fn new(
        record_type: String,
        id: RecordId,
        data: Value,
        provenance: Provenance,
    ) -> Self {
        Self {
            record_type,
            id,
            data,
            provenance,
        }
    }
}

/// How a filter compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldOp {
    /// Equal.
    Eq,
    /// Less than.
    Lt,
    /// At most.
    Lte,
    /// More than.
    Gt,
    /// At least.
    Gte,
}

/// A value to compare a field with.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    /// A string.
    Text(String),
    /// An integer.
    Integer(i64),
    /// A number.
    Number(f64),
    /// True or false.
    Bool(bool),
    /// A time.
    Timestamp(DateTime<Utc>),
}

/// One condition on a declared field.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct FieldFilter {
    /// The field, which must be declared filterable.
    pub field: String,
    /// How to compare.
    pub op: FieldOp,
    /// What to compare with.
    pub value: FieldValue,
}

impl FieldFilter {
    /// `field op value`.
    #[must_use]
    pub fn new(field: impl Into<String>, op: FieldOp, value: FieldValue) -> Self {
        Self {
            field: field.into(),
            op,
            value,
        }
    }
}

/// Which records to read. All filters must hold.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RecordQuery {
    /// The type to read.
    pub record_type: String,
    /// The conditions.
    pub filters: Vec<FieldFilter>,
    /// Which slice to return, in id order.
    pub page: PageRequest,
}

impl RecordQuery {
    /// Every record of a type, a page at a time.
    #[must_use]
    pub fn new(record_type: impl Into<String>, page: PageRequest) -> Self {
        Self {
            record_type: record_type.into(),
            filters: Vec::new(),
            page,
        }
    }

    /// The same query with these conditions as well.
    #[must_use]
    pub fn with_filters(mut self, filters: impl IntoIterator<Item = FieldFilter>) -> Self {
        self.filters.extend(filters);
        self
    }

    /// The same query with one more condition.
    #[must_use]
    pub fn with_filter(mut self, field: impl Into<String>, op: FieldOp, value: FieldValue) -> Self {
        self.filters.push(FieldFilter::new(field, op, value));
        self
    }
}

/// Which way to follow a relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// From the given record to what it links to.
    Outgoing,
    /// From the given record back to what links to it.
    Incoming,
}
