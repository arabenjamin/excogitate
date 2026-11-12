//! The capability calls a plugin process makes back to the host, as JSON.
//! Both ends use these types, so they can't drift apart.
//!
//! A call is the JSON-RPC request `capabilities/call`, with `run` naming the
//! tool call it's made for and `call` saying what it asks. The host answers
//! only for a run it's serving on that connection, and uses its own record
//! of that run's privacy class and budget, never the plugin's.

use chrono::{DateTime, Utc};
use protocol::{Actor, DataClass, Trust};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use store::{
    Direction, FieldFilter, FieldOp, FieldValue, Page, PageRequest, Provenance, Record, RecordId,
    RecordQuery,
};
use uuid::Uuid;

use crate::{CapabilityError, HttpRequest, HttpResponse, ModelReply, PluginCapability};

/// The JSON-RPC method a capability call uses.
pub const CAPABILITY_METHOD: &str = "capabilities/call";

/// JSON-RPC error codes for a refused or failed capability call.
pub const REFUSED_CODE: i64 = -32001;
/// The plugin wasn't granted the capability.
pub const NOT_GRANTED_CODE: i64 = -32002;
/// The host couldn't do it.
pub const FAILED_CODE: i64 = -32003;

/// The longest type, relation, field or actor name on the wire.
pub const NAME: usize = 128;
/// The most conditions in one query.
pub const MAX_FILTERS: usize = 32;
/// The longest text a filter compares with.
pub const MAX_TEXT_VALUE: usize = 4096;
/// The longest prompt a plugin can send.
pub const MAX_PROMPT: usize = 200_000;
/// The longest text that comes back in an answer.
pub const MAX_ANSWER: usize = 1_000_000;

/// A header, as name and value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireHeader {
    /// Its name.
    #[schemars(length(min = 1, max = 64))]
    pub name: String,
    /// Its value.
    #[schemars(length(max = 1024))]
    pub value: String,
}

/// The parameters of a `capabilities/call` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequest {
    /// The token of the tool call it's made for, from that call's context.
    pub token: Uuid,
    /// What it asks.
    pub call: CapabilityCall,
}

/// One record of a batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireRecordWrite {
    /// The type.
    #[schemars(length(min = 1, max = NAME))]
    pub record_type: String,
    /// The id to replace, or none for a new record.
    pub id: Option<Uuid>,
    /// Its data.
    #[schemars(extend("x-extension" = "checked against the record type's own schema"))]
    pub data: Value,
}

/// One link of a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireLink {
    /// The relation.
    #[schemars(length(min = 1, max = NAME))]
    pub relation: String,
    /// The record it links from.
    pub from: Uuid,
    /// The record it links to.
    pub to: Uuid,
}

/// What a plugin asks, by operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityCall {
    /// `RecordsCapability::put`.
    RecordsPut {
        /// The type.
        #[schemars(length(min = 1, max = NAME))]
        record_type: String,
        /// The id to replace, or none for a new record.
        id: Option<Uuid>,
        /// Its data.
        #[schemars(extend("x-extension" = "checked against the record type's own schema"))]
        data: Value,
    },
    /// `RecordsCapability::get`.
    RecordsGet {
        /// The type.
        #[schemars(length(min = 1, max = NAME))]
        record_type: String,
        /// The id.
        id: Uuid,
    },
    /// `RecordsCapability::query`.
    RecordsQuery {
        /// The type.
        #[schemars(length(min = 1, max = NAME))]
        record_type: String,
        /// The conditions, all of which must hold.
        #[schemars(length(max = MAX_FILTERS))]
        filters: Vec<WireFilter>,
        /// The most to return.
        limit: u32,
        /// How many to skip.
        offset: u32,
    },
    /// `RecordsCapability::delete`.
    RecordsDelete {
        /// The type.
        #[schemars(length(min = 1, max = NAME))]
        record_type: String,
        /// The id.
        id: Uuid,
    },
    /// `RecordsCapability::link`.
    RecordsLink {
        /// The relation.
        #[schemars(length(min = 1, max = NAME))]
        relation: String,
        /// The record it links from.
        from: Uuid,
        /// The record it links to.
        to: Uuid,
    },
    /// `RecordsCapability::put_many`: written as one unit.
    RecordsPutMany {
        /// The records, in order.
        #[schemars(length(max = crate::MAX_BATCH))]
        records: Vec<WireRecordWrite>,
    },
    /// `RecordsCapability::link_many`: made as one unit.
    RecordsLinkMany {
        /// The links.
        #[schemars(length(max = crate::MAX_BATCH))]
        links: Vec<WireLink>,
    },
    /// `RecordsCapability::neighbors`.
    RecordsNeighbors {
        /// The relation.
        #[schemars(length(min = 1, max = NAME))]
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
        #[schemars(length(min = 1, max = 16))]
        method: String,
        /// The URL.
        #[schemars(length(min = 1, max = 2048))]
        url: String,
        /// Headers, as name and value.
        #[schemars(length(max = 16))]
        headers: Vec<WireHeader>,
        /// The body.
        #[schemars(length(max = 64_000))]
        body: Option<String>,
    },
    /// `ModelCapability::complete`.
    Model {
        /// What to ask.
        #[schemars(length(min = 1, max = MAX_PROMPT))]
        prompt: String,
    },
    /// `JobsCapability::run_workflow`.
    JobsRunWorkflow {
        /// The workflow.
        #[schemars(length(min = 1, max = 64))]
        workflow: String,
        /// Its inputs.
        #[schemars(extend("x-extension" = "checked against the workflow's inputs schema"))]
        inputs: Value,
        /// Higher runs first.
        #[schemars(range(min = -1000, max = 1000))]
        priority: i32,
        /// Starts it once per key.
        #[schemars(length(min = 1, max = crate::MAX_IDEMPOTENCY_KEY))]
        idempotency_key: Option<String>,
    },
    /// `JobsCapability::job`.
    JobsGet {
        /// The job.
        job_id: Uuid,
    },
    /// `MemoryCapability::store`.
    MemoryStore {
        /// A short title.
        #[schemars(length(max = crate::MAX_NOTE_TITLE))]
        title: String,
        /// The text.
        #[schemars(length(min = 1, max = crate::MAX_NOTE_TEXT))]
        text: String,
        /// Tags.
        #[schemars(
            length(max = crate::MAX_NOTE_TAGS),
            inner(length(max = crate::MAX_NOTE_TAG))
        )]
        tags: Vec<String>,
        /// The URL it came from.
        #[schemars(length(max = crate::MAX_NOTE_URL))]
        source_url: Option<String>,
    },
}

/// A filter on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireFilter {
    /// The field.
    #[schemars(length(min = 1, max = NAME))]
    pub field: String,
    /// `eq`, `lt`, `lte`, `gt` or `gte`.
    pub op: WireOp,
    /// The value, typed.
    pub value: WireValue,
}

/// A comparison on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[allow(missing_docs, reason = "the variants are FieldValue's")]
pub enum WireValue {
    Text(#[schemars(length(max = MAX_TEXT_VALUE))] String),
    Integer(i64),
    Number(f64),
    Bool(bool),
    Timestamp(DateTime<Utc>),
}

/// A direction on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs, reason = "the variants are Direction's")]
pub enum WireDirection {
    Outgoing,
    Incoming,
}

/// A record on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireRecord {
    /// Its type.
    #[schemars(length(min = 1, max = NAME))]
    pub record_type: String,
    /// Its id.
    pub id: Uuid,
    /// Its data.
    #[schemars(extend("x-extension" = "data meeting the record type's own schema"))]
    pub data: Value,
    /// `private` or `public`.
    #[schemars(length(max = 16))]
    pub data_class: String,
    /// `trusted` or `untrusted`.
    #[schemars(length(max = 16))]
    pub trust: String,
    /// Who stored it.
    #[schemars(length(max = NAME))]
    pub stored_by: String,
    /// When.
    pub stored_at: DateTime<Utc>,
}

/// A page of records on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WirePage {
    /// The records.
    #[schemars(length(max = 1000))]
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
    let mut query = RecordQuery::new(record_type, page_of(limit, offset)?);
    query.filters = filters
        .into_iter()
        .map(|filter| FieldFilter::new(filter.field, filter.op.into(), filter.value.into()))
        .collect();
    Ok(query)
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
    Ok(Record::new(
        wire.record_type,
        RecordId::from_uuid(wire.id),
        wire.data,
        Provenance {
            data_class,
            trust,
            stored_by: Actor::new(wire.stored_by),
            stored_at: wire.stored_at,
        },
    ))
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
        headers: request
            .headers
            .into_iter()
            .map(|(name, value)| WireHeader { name, value })
            .collect(),
        body: request.body,
    }
}

/// An HTTP response on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs, reason = "the fields are HttpResponse's")]
pub struct WireHttpResponse {
    pub status: u16,
    #[schemars(length(max = MAX_ANSWER))]
    pub body: String,
    pub truncated: bool,
    #[schemars(length(max = 1024))]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs, reason = "the fields are ModelReply's")]
pub struct WireModelReply {
    #[schemars(length(max = MAX_ANSWER))]
    pub text: String,
    #[schemars(length(max = NAME))]
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

/// A started run, on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs, reason = "the fields are JobStarted's")]
pub struct WireJobStarted {
    pub job_id: Uuid,
    pub already_existed: bool,
}

/// Where a job stands, on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs, reason = "the variants are JobPhase's")]
pub enum WireJobPhase {
    Queued,
    Running,
    Succeeded,
    Dead,
}

/// A job's state, on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs, reason = "the fields are JobState's")]
pub struct WireJobState {
    pub job_id: Uuid,
    pub phase: WireJobPhase,
    #[schemars(length(max = 4096))]
    pub last_error: Option<String>,
    #[schemars(extend("x-extension" = "what the workflow run produced"))]
    pub result: Option<Value>,
}

impl From<crate::JobState> for WireJobState {
    fn from(state: crate::JobState) -> Self {
        Self {
            job_id: state.job_id,
            phase: match state.phase {
                crate::JobPhase::Queued => WireJobPhase::Queued,
                crate::JobPhase::Running => WireJobPhase::Running,
                crate::JobPhase::Succeeded => WireJobPhase::Succeeded,
                crate::JobPhase::Dead => WireJobPhase::Dead,
            },
            last_error: state.last_error,
            result: state.result,
        }
    }
}

impl From<WireJobState> for crate::JobState {
    fn from(state: WireJobState) -> Self {
        let phase = match state.phase {
            WireJobPhase::Queued => crate::JobPhase::Queued,
            WireJobPhase::Running => crate::JobPhase::Running,
            WireJobPhase::Succeeded => crate::JobPhase::Succeeded,
            WireJobPhase::Dead => crate::JobPhase::Dead,
        };
        Self::new(state.job_id, phase).with_outcome(state.last_error, state.result)
    }
}

/// What storing a note did, on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs, reason = "the fields are NoteStored's")]
pub struct WireNoteStored {
    pub id: Uuid,
    pub created: bool,
    pub chunks: u32,
    pub unembedded_chunks: u32,
    pub cost_usd_micros: u64,
}

impl From<crate::NoteStored> for WireNoteStored {
    fn from(stored: crate::NoteStored) -> Self {
        Self {
            id: stored.id,
            created: stored.created,
            chunks: stored.chunks,
            unembedded_chunks: stored.unembedded_chunks,
            cost_usd_micros: stored.cost_usd_micros,
        }
    }
}

impl From<WireNoteStored> for crate::NoteStored {
    fn from(stored: WireNoteStored) -> Self {
        Self::new(
            stored.id,
            stored.created,
            stored.chunks,
            stored.unembedded_chunks,
        )
        .with_cost(stored.cost_usd_micros)
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

/// The message a capability error travels with: its detail, without the
/// prefix its kind adds, since the code carries the kind.
#[must_use]
pub fn error_message(error: &CapabilityError) -> String {
    match error {
        CapabilityError::NotGranted(capability) => serde_json::to_value(capability)
            .ok()
            .and_then(|name| name.as_str().map(str::to_owned))
            .unwrap_or_default(),
        CapabilityError::Refused(detail) | CapabilityError::Failed(detail) => detail.clone(),
    }
}

/// A capability error from a JSON-RPC error. The message is the host's
/// own, already bounded.
#[must_use]
pub fn error_of(code: i64, message: &str) -> CapabilityError {
    let not_granted =
        || serde_json::from_value::<PluginCapability>(Value::String(message.to_owned())).ok();
    match code {
        REFUSED_CODE => CapabilityError::Refused(message.to_owned()),
        NOT_GRANTED_CODE => not_granted().map_or_else(
            || CapabilityError::Failed(message.to_owned()),
            CapabilityError::NotGranted,
        ),
        _ => CapabilityError::Failed(message.to_owned()),
    }
}

/// The context a tool call carries to a plugin process, in `_meta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireContext {
    /// The run.
    pub run: Uuid,
    /// `true` if the run's data is public.
    pub public: bool,
    /// What's left of the run's budget.
    pub budget_usd_micros: u64,
    /// The host's token for this tool call. A capability call made for it
    /// must carry it back, and it's good only while the call runs.
    pub call: Uuid,
}

/// The `_meta` key a tool call's context travels under.
pub const CONTEXT_META: &str = "context";

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test: a panic is how it fails")]
mod tests {
    use chrono::TimeZone;

    use super::*;

    /// Through JSON and back, as it crosses the connection.
    fn across<T: Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
        serde_json::from_str(&serde_json::to_string(value).unwrap()).unwrap()
    }

    fn record(data_class: DataClass, trust: Trust) -> Record {
        Record::new(
            "demo.a".to_owned(),
            RecordId::generate(),
            serde_json::json!({ "a": 1 }),
            Provenance {
                data_class,
                trust,
                stored_by: Actor::new("plugin:demo"),
                stored_at: Utc.with_ymd_and_hms(2026, 11, 11, 12, 0, 0).unwrap(),
            },
        )
    }

    #[test]
    fn a_record_and_its_labels_survive_the_trip() {
        for (class, trust) in [
            (DataClass::Private, Trust::Untrusted),
            (DataClass::Public, Trust::Trusted),
        ] {
            let sent = record(class, trust);
            let page = Page {
                items: vec![sent.clone()],
                has_more: true,
            };
            assert_eq!(page_from(across(&wire_page(page.clone()))).unwrap(), page);
            assert_eq!(record_of(across(&wire_record(sent.clone()))).unwrap(), sent);
        }
    }

    #[test]
    fn a_record_with_a_label_it_doesnt_know_is_refused() {
        let mut wire = wire_record(record(DataClass::Private, Trust::Untrusted));
        wire.trust = "sort of".to_owned();
        assert!(record_of(wire.clone()).is_err());
        wire.trust = "trusted".to_owned();
        wire.data_class = "secret".to_owned();
        assert!(record_of(wire).is_err());
    }

    #[test]
    fn a_query_survives_the_trip_with_every_kind_of_value() {
        let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let query = RecordQuery::new("demo.a", PageRequest::new(20, 40).unwrap())
            .with_filter("name", FieldOp::Eq, FieldValue::Text("x".to_owned()))
            .with_filter("size", FieldOp::Lt, FieldValue::Integer(3))
            .with_filter("score", FieldOp::Gte, FieldValue::Number(0.5))
            .with_filter("done", FieldOp::Lte, FieldValue::Bool(true))
            .with_filter("at", FieldOp::Gt, FieldValue::Timestamp(at));
        let CapabilityCall::RecordsQuery {
            record_type,
            filters,
            limit,
            offset,
        } = across(&query_call(query.clone()))
        else {
            panic!("not a query");
        };
        assert_eq!(
            query_of(record_type, filters, limit, offset).unwrap(),
            query
        );
    }

    #[test]
    fn a_page_out_of_bounds_is_refused() {
        assert!(matches!(page_of(0, 0), Err(CapabilityError::Refused(_))));
        assert!(matches!(
            query_of("demo.a".to_owned(), Vec::new(), 100_000, 0),
            Err(CapabilityError::Refused(_))
        ));
    }

    #[test]
    fn http_and_model_answers_survive_the_trip() {
        let response = HttpResponse::new(206, "part")
            .with_truncated(true)
            .with_note(Some("cut".to_owned()));
        let back: HttpResponse = across(&WireHttpResponse::from(response.clone())).into();
        assert_eq!(back, response);
        let reply = ModelReply::new("hi", "m", 12);
        let back: ModelReply = across(&WireModelReply::from(reply.clone())).into();
        assert_eq!(back, reply);
        let request = HttpRequest::get("https://example.com/")
            .with_header("accept", "text/plain")
            .with_body("b");
        assert_eq!(
            across(&http_call(request)),
            CapabilityCall::Http {
                method: "GET".to_owned(),
                url: "https://example.com/".to_owned(),
                headers: vec![WireHeader {
                    name: "accept".to_owned(),
                    value: "text/plain".to_owned()
                }],
                body: Some("b".to_owned()),
            }
        );
    }

    #[test]
    fn each_kind_of_capability_error_keeps_its_kind_and_detail() {
        for error in [
            CapabilityError::NotGranted(PluginCapability::Http),
            CapabilityError::Refused("not yours".to_owned()),
            CapabilityError::Failed("store down".to_owned()),
        ] {
            let back = error_of(error_code(&error), &error_message(&error));
            assert_eq!(back, error);
        }
        // An unknown code is a failure, whatever it says.
        assert_eq!(
            error_of(-1, "odd"),
            CapabilityError::Failed("odd".to_owned())
        );
    }

    #[test]
    fn a_call_with_a_key_it_doesnt_know_is_refused() {
        let call = serde_json::json!({ "op": "model", "prompt": "hi", "model": "big" });
        assert!(serde_json::from_value::<CapabilityCall>(call).is_err());
    }
}
