//! The capability calls a plugin process makes back to the host, as JSON.
//! Both ends use these types, so they can't drift apart.
//!
//! A call is the JSON-RPC request `capabilities/call`, with `run` naming the
//! tool call it's made for and `call` saying what it asks. The host answers
//! only for a run it's serving on that connection, and uses its own record
//! of that run's privacy class and budget, never the plugin's.

use chrono::{DateTime, Utc};
use protocol::{Actor, DataClass, Trust};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use store::{
    Direction, FieldFilter, FieldOp, FieldValue, Page, PageRequest, Provenance, Record, RecordId,
    RecordQuery,
};
use uuid::Uuid;

use crate::{CapabilityError, HttpRequest, HttpResponse, ModelReply};

/// The JSON-RPC method a capability call uses.
pub const CAPABILITY_METHOD: &str = "capabilities/call";

/// JSON-RPC error codes for a refused or failed capability call.
pub const REFUSED_CODE: i64 = -32001;
/// The plugin wasn't granted the capability.
pub const NOT_GRANTED_CODE: i64 = -32002;
/// The host couldn't do it.
pub const FAILED_CODE: i64 = -32003;

/// What a plugin asks, by operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityCall {
    /// `RecordsCapability::put`.
    RecordsPut {
        /// The type.
        record_type: String,
        /// The id to replace, or none for a new record.
        id: Option<Uuid>,
        /// Its data.
        data: Value,
    },
    /// `RecordsCapability::get`.
    RecordsGet {
        /// The type.
        record_type: String,
        /// The id.
        id: Uuid,
    },
    /// `RecordsCapability::query`.
    RecordsQuery {
        /// The type.
        record_type: String,
        /// The conditions, all of which must hold.
        filters: Vec<WireFilter>,
        /// The most to return.
        limit: u32,
        /// How many to skip.
        offset: u32,
    },
    /// `RecordsCapability::delete`.
    RecordsDelete {
        /// The type.
        record_type: String,
        /// The id.
        id: Uuid,
    },
    /// `RecordsCapability::link`.
    RecordsLink {
        /// The relation.
        relation: String,
        /// The record it links from.
        from: Uuid,
        /// The record it links to.
        to: Uuid,
    },
    /// `RecordsCapability::neighbors`.
    RecordsNeighbors {
        /// The relation.
        relation: String,
        /// The record to start from.
        id: Uuid,
        /// `outgoing` or `incoming`.
        direction: WireDirection,
        /// The most to return.
        limit: u32,
        /// How many to skip.
        offset: u32,
    },
    /// `HttpCapability::request`.
    Http {
        /// The method.
        method: String,
        /// The URL.
        url: String,
        /// Headers, as name and value.
        headers: Vec<(String, String)>,
        /// The body.
        body: Option<String>,
    },
    /// `ModelCapability::complete`.
    Model {
        /// What to ask.
        prompt: String,
    },
}

/// A filter on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireFilter {
    /// The field.
    pub field: String,
    /// `eq`, `lt`, `lte`, `gt` or `gte`.
    pub op: WireOp,
    /// The value, typed.
    pub value: WireValue,
}

/// A comparison on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs, reason = "the variants are FieldOp's")]
pub enum WireOp {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
}

/// A value on the wire, tagged with its type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
#[allow(missing_docs, reason = "the variants are FieldValue's")]
pub enum WireValue {
    Text(String),
    Integer(i64),
    Number(f64),
    Bool(bool),
    Timestamp(DateTime<Utc>),
}

/// A direction on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs, reason = "the variants are Direction's")]
pub enum WireDirection {
    Outgoing,
    Incoming,
}

/// A record on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRecord {
    /// Its type.
    pub record_type: String,
    /// Its id.
    pub id: Uuid,
    /// Its data.
    pub data: Value,
    /// `private` or `public`.
    pub data_class: String,
    /// `trusted` or `untrusted`.
    pub trust: String,
    /// Who stored it.
    pub stored_by: String,
    /// When.
    pub stored_at: DateTime<Utc>,
}

/// A page of records on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WirePage {
    /// The records.
    pub items: Vec<WireRecord>,
    /// Whether there are more.
    pub has_more: bool,
}

impl From<FieldOp> for WireOp {
    fn from(op: FieldOp) -> Self {
        match op {
            FieldOp::Eq => Self::Eq,
            FieldOp::Lt => Self::Lt,
            FieldOp::Lte => Self::Lte,
            FieldOp::Gt => Self::Gt,
            FieldOp::Gte => Self::Gte,
        }
    }
}

impl From<WireOp> for FieldOp {
    fn from(op: WireOp) -> Self {
        match op {
            WireOp::Eq => Self::Eq,
            WireOp::Lt => Self::Lt,
            WireOp::Lte => Self::Lte,
            WireOp::Gt => Self::Gt,
            WireOp::Gte => Self::Gte,
        }
    }
}

impl From<FieldValue> for WireValue {
    fn from(value: FieldValue) -> Self {
        match value {
            FieldValue::Text(text) => Self::Text(text),
            FieldValue::Integer(number) => Self::Integer(number),
            FieldValue::Number(number) => Self::Number(number),
            FieldValue::Bool(flag) => Self::Bool(flag),
            FieldValue::Timestamp(time) => Self::Timestamp(time),
        }
    }
}

impl From<WireValue> for FieldValue {
    fn from(value: WireValue) -> Self {
        match value {
            WireValue::Text(text) => Self::Text(text),
            WireValue::Integer(number) => Self::Integer(number),
            WireValue::Number(number) => Self::Number(number),
            WireValue::Bool(flag) => Self::Bool(flag),
            WireValue::Timestamp(time) => Self::Timestamp(time),
        }
    }
}

impl From<Direction> for WireDirection {
    fn from(direction: Direction) -> Self {
        match direction {
            Direction::Outgoing => Self::Outgoing,
            Direction::Incoming => Self::Incoming,
        }
    }
}

impl From<WireDirection> for Direction {
    fn from(direction: WireDirection) -> Self {
        match direction {
            WireDirection::Outgoing => Self::Outgoing,
            WireDirection::Incoming => Self::Incoming,
        }
    }
}

/// A query as a call.
#[must_use]
pub fn query_call(query: RecordQuery) -> CapabilityCall {
    CapabilityCall::RecordsQuery {
        record_type: query.record_type,
        filters: query
            .filters
            .into_iter()
            .map(|filter| WireFilter {
                field: filter.field,
                op: filter.op.into(),
                value: filter.value.into(),
            })
            .collect(),
        limit: query.page.limit(),
        offset: query.page.offset(),
    }
}

/// A call's query, back as a store query.
///
/// # Errors
///
/// [`CapabilityError::Refused`] if the page is out of bounds.
pub fn query_of(
    record_type: String,
    filters: Vec<WireFilter>,
    limit: u32,
    offset: u32,
) -> Result<RecordQuery, CapabilityError> {
    Ok(RecordQuery {
        record_type,
        filters: filters
            .into_iter()
            .map(|filter| FieldFilter {
                field: filter.field,
                op: filter.op.into(),
                value: filter.value.into(),
            })
            .collect(),
        page: page_of(limit, offset)?,
    })
}

/// A page request from the wire.
///
/// # Errors
///
/// [`CapabilityError::Refused`] if it's out of bounds.
pub fn page_of(limit: u32, offset: u32) -> Result<PageRequest, CapabilityError> {
    PageRequest::new(limit, offset).map_err(|error| CapabilityError::Refused(error.to_string()))
}

/// A record for the wire.
#[must_use]
pub fn wire_record(record: Record) -> WireRecord {
    WireRecord {
        record_type: record.record_type,
        id: record.id.as_uuid(),
        data: record.data,
        data_class: match record.provenance.data_class {
            DataClass::Private => "private",
            DataClass::Public => "public",
        }
        .to_owned(),
        trust: match record.provenance.trust {
            Trust::Trusted => "trusted",
            Trust::Untrusted => "untrusted",
        }
        .to_owned(),
        stored_by: record.provenance.stored_by.as_str().to_owned(),
        stored_at: record.provenance.stored_at,
    }
}

/// A record from the wire.
///
/// # Errors
///
/// [`CapabilityError::Failed`] if its labels aren't known ones.
pub fn record_of(wire: WireRecord) -> Result<Record, CapabilityError> {
    let data_class = match wire.data_class.as_str() {
        "private" => DataClass::Private,
        "public" => DataClass::Public,
        other => {
            return Err(CapabilityError::Failed(format!(
                "unknown data class {other}"
            )));
        }
    };
    let trust = match wire.trust.as_str() {
        "trusted" => Trust::Trusted,
        "untrusted" => Trust::Untrusted,
        other => return Err(CapabilityError::Failed(format!("unknown trust {other}"))),
    };
    Ok(Record {
        record_type: wire.record_type,
        id: RecordId::from_uuid(wire.id),
        data: wire.data,
        provenance: Provenance {
            data_class,
            trust,
            stored_by: Actor::new(wire.stored_by),
            stored_at: wire.stored_at,
        },
    })
}

/// A page for the wire.
#[must_use]
pub fn wire_page(page: Page<Record>) -> WirePage {
    WirePage {
        items: page.items.into_iter().map(wire_record).collect(),
        has_more: page.has_more,
    }
}

/// A page from the wire.
///
/// # Errors
///
/// [`CapabilityError::Failed`] if a record's labels aren't known ones.
pub fn page_from(wire: WirePage) -> Result<Page<Record>, CapabilityError> {
    Ok(Page {
        items: wire
            .items
            .into_iter()
            .map(record_of)
            .collect::<Result<_, _>>()?,
        has_more: wire.has_more,
    })
}

/// An HTTP request as a call.
#[must_use]
pub fn http_call(request: HttpRequest) -> CapabilityCall {
    CapabilityCall::Http {
        method: request.method,
        url: request.url,
        headers: request.headers,
        body: request.body,
    }
}

/// An HTTP response on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs, reason = "the fields are HttpResponse's")]
pub struct WireHttpResponse {
    pub status: u16,
    pub body: String,
    pub truncated: bool,
    pub note: Option<String>,
}

impl From<HttpResponse> for WireHttpResponse {
    fn from(response: HttpResponse) -> Self {
        Self {
            status: response.status,
            body: response.body,
            truncated: response.truncated,
            note: response.note,
        }
    }
}

impl From<WireHttpResponse> for HttpResponse {
    fn from(response: WireHttpResponse) -> Self {
        Self {
            status: response.status,
            body: response.body,
            truncated: response.truncated,
            note: response.note,
        }
    }
}

/// A model's reply on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs, reason = "the fields are ModelReply's")]
pub struct WireModelReply {
    pub text: String,
    pub model: String,
    pub cost_usd_micros: u64,
}

impl From<ModelReply> for WireModelReply {
    fn from(reply: ModelReply) -> Self {
        Self {
            text: reply.text,
            model: reply.model,
            cost_usd_micros: reply.cost_usd_micros,
        }
    }
}

impl From<WireModelReply> for ModelReply {
    fn from(reply: WireModelReply) -> Self {
        Self {
            text: reply.text,
            model: reply.model,
            cost_usd_micros: reply.cost_usd_micros,
        }
    }
}

/// The JSON-RPC error code for a capability error.
#[must_use]
pub const fn error_code(error: &CapabilityError) -> i64 {
    match error {
        CapabilityError::NotGranted(_) => NOT_GRANTED_CODE,
        CapabilityError::Refused(_) => REFUSED_CODE,
        CapabilityError::Failed(_) => FAILED_CODE,
    }
}

/// A capability error from a JSON-RPC error. The message is the host's
/// own, already bounded.
#[must_use]
pub fn error_of(code: i64, message: &str) -> CapabilityError {
    match code {
        REFUSED_CODE => CapabilityError::Refused(message.to_owned()),
        _ => CapabilityError::Failed(message.to_owned()),
    }
}

/// The context a tool call carries to a plugin process, in `_meta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireContext {
    /// The run.
    pub run: Uuid,
    /// `true` if the run's data is public.
    pub public: bool,
    /// What's left of the run's budget.
    pub budget_usd_micros: u64,
}

/// The `_meta` key a tool call's context travels under.
pub const CONTEXT_META: &str = "context";
