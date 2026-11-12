//! Between Rust types and graph properties. Everything read back is checked,
//! so a bad node is an error rather than a guess.
//!
//! Each kind has a projection (`NOTE`, `JOB`, ...) that returns its
//! properties as named columns, and a decoder that reads those columns. A
//! property that's absent comes back as null, so optional fields read as
//! `None` instead of failing.

use std::collections::HashMap;
use std::fmt::Display;
use std::str::FromStr;

use chrono::{DateTime, FixedOffset, Utc};
use neo4rs::{BoltType, Row};
use protocol::{Actor, DataClass, Trust};
use serde::de::DeserializeOwned;
use serde_json::Value;
use store::{
    Chunk, ChunkId, Definition, DefinitionKey, DefinitionKind, FieldValue, Job, JobId, JobStatus,
    Lease, Note, NoteId, Provenance, Record, RecordId, StoreError, Task, TaskId, TaskStatus,
    WorkingEntry,
};
use uuid::Uuid;

/// A store failure, with its message kept short.
pub(crate) fn backend(error: impl Display) -> StoreError {
    StoreError::Backend(schema::bounded(&error.to_string()))
}

/// A driver error as a store error. A uniqueness constraint that fails is a
/// conflict: two writers raced to create the same thing.
pub(crate) fn from_neo(error: neo4rs::Error) -> StoreError {
    if let neo4rs::Error::Neo4j(server) = &error
        && server.code() == "Neo.ClientError.Schema.ConstraintValidationFailed"
    {
        return StoreError::Conflict(schema::bounded(server.message()));
    }
    backend(error)
}

/// Whether a failure is one Neo4j says to retry, such as a deadlock.
pub(crate) fn is_transient(error: &StoreError) -> bool {
    matches!(error, StoreError::Backend(message) if message.contains("Neo.TransientError."))
}

fn bad(column: &str, problem: impl Display) -> StoreError {
    StoreError::Backend(schema::bounded(&format!(
        "stored {column} is unreadable: {problem}"
    )))
}

/// A column, of a type the driver can read.
pub(crate) fn get<T: DeserializeOwned>(row: &Row, column: &str) -> Result<T, StoreError> {
    row.get::<T>(column).map_err(|error| bad(column, error))
}

fn uuid(row: &Row, column: &str) -> Result<Uuid, StoreError> {
    Uuid::from_str(&get::<String>(row, column)?).map_err(|error| bad(column, error))
}

fn json(row: &Row, column: &str) -> Result<Value, StoreError> {
    serde_json::from_str(&get::<String>(row, column)?).map_err(|error| bad(column, error))
}

fn optional_json(row: &Row, column: &str) -> Result<Option<Value>, StoreError> {
    get::<Option<String>>(row, column)?
        .map(|text| serde_json::from_str(&text).map_err(|error| bad(column, error)))
        .transpose()
}

fn number<N: TryFrom<i64>>(row: &Row, column: &str) -> Result<N, StoreError> {
    N::try_from(get::<i64>(row, column)?).map_err(|_| bad(column, "number out of range"))
}

fn time(row: &Row, column: &str) -> Result<DateTime<Utc>, StoreError> {
    Ok(get::<DateTime<FixedOffset>>(row, column)?.with_timezone(&Utc))
}

fn optional_time(row: &Row, column: &str) -> Result<Option<DateTime<Utc>>, StoreError> {
    Ok(get::<Option<DateTime<FixedOffset>>>(row, column)?.map(|time| time.with_timezone(&Utc)))
}

/// A time as a parameter. Neo4j stores it as a native `DATETIME` in UTC.
pub(crate) fn at(time: DateTime<Utc>) -> BoltType {
    time.fixed_offset().into()
}

/// An optional time as a parameter; `None` is null, which removes it.
pub(crate) fn maybe_at(time: Option<DateTime<Utc>>) -> BoltType {
    time.map_or(BoltType::Null(neo4rs::BoltNull), at)
}

/// A JSON value as a parameter. Values are stored as JSON text: Neo4j
/// properties can't nest maps.
pub(crate) fn json_text(value: &Value) -> String {
    value.to_string()
}

/// A count or version as a parameter.
pub(crate) fn int(value: impl Into<u64>) -> Result<i64, StoreError> {
    i64::try_from(value.into()).map_err(|_| StoreError::Invalid("a number is too large".to_owned()))
}

macro_rules! text_enum {
    ($type:ty, $to:ident, $from:ident, $($variant:path => $name:literal),+ $(,)?) => {
        pub(crate) const fn $to(value: $type) -> &'static str {
            match value { $($variant => $name),+ }
        }

        fn $from(row: &Row, column: &str) -> Result<$type, StoreError> {
            match get::<String>(row, column)?.as_str() {
                $($name => Ok($variant),)+
                other => Err(bad(column, format!("unknown value {other}"))),
            }
        }
    };
}

text_enum!(DataClass, data_class_name, data_class_at,
    DataClass::Private => "private", DataClass::Public => "public");
text_enum!(Trust, trust_name, trust_at,
    Trust::Trusted => "trusted", Trust::Untrusted => "untrusted");
text_enum!(TaskStatus, task_status_name, task_status_at,
    TaskStatus::Pending => "pending", TaskStatus::Active => "active",
    TaskStatus::Done => "done", TaskStatus::Cancelled => "cancelled");
text_enum!(JobStatus, job_status_name, job_status_at,
    JobStatus::Queued => "queued", JobStatus::Claimed => "claimed",
    JobStatus::Succeeded => "succeeded", JobStatus::Dead => "dead");
text_enum!(DefinitionKind, definition_kind_name, definition_kind_at,
    DefinitionKind::Agent => "agent", DefinitionKind::Workflow => "workflow",
    DefinitionKind::Schedule => "schedule");

/// Provenance as properties, for a `SET x += $map`.
pub(crate) fn provenance_props(provenance: &Provenance) -> HashMap<String, BoltType> {
    HashMap::from([
        (
            "data_class".to_owned(),
            data_class_name(provenance.data_class).into(),
        ),
        ("trust".to_owned(), trust_name(provenance.trust).into()),
        ("stored_by".to_owned(), provenance.stored_by.as_str().into()),
        ("stored_at".to_owned(), at(provenance.stored_at)),
    ])
}

fn provenance(row: &Row) -> Result<Provenance, StoreError> {
    Ok(Provenance {
        data_class: data_class_at(row, "data_class")?,
        trust: trust_at(row, "trust")?,
        stored_by: Actor::new(get::<String>(row, "stored_by")?),
        stored_at: time(row, "stored_at")?,
    })
}

/// The provenance columns of the node bound to `n`.
const PROVENANCE: &str = "n.data_class AS data_class, n.trust AS trust, n.stored_by AS stored_by, n.stored_at AS stored_at";

/// The columns [`note`] reads, for a note bound to `n`.
pub(crate) fn note_columns() -> String {
    format!(
        "n.id AS id, n.title AS title, n.body AS body, n.tags AS tags, \
         n.source_url AS source_url, n.content_hash AS content_hash, n.version AS version, \
         n.updated_at AS updated_at, {PROVENANCE}"
    )
}

pub(crate) fn note(row: &Row) -> Result<Note, StoreError> {
    Ok(Note {
        id: NoteId::from_uuid(uuid(row, "id")?),
        title: get(row, "title")?,
        body: get(row, "body")?,
        tags: get(row, "tags")?,
        source_url: get(row, "source_url")?,
        content_hash: get(row, "content_hash")?,
        provenance: provenance(row)?,
        version: number(row, "version")?,
        updated_at: time(row, "updated_at")?,
    })
}

/// A note's own properties, for `SET n += $props`.
pub(crate) fn note_props(
    note: &Note,
    version: u64,
) -> Result<HashMap<String, BoltType>, StoreError> {
    let mut props = provenance_props(&note.provenance);
    props.extend([
        ("id".to_owned(), note.id.to_string().into()),
        ("title".to_owned(), note.title.clone().into()),
        ("body".to_owned(), note.body.clone().into()),
        ("tags".to_owned(), note.tags.clone().into()),
        ("source_url".to_owned(), note.source_url.clone().into()),
        ("content_hash".to_owned(), note.content_hash.clone().into()),
        ("version".to_owned(), int(version)?.into()),
        ("updated_at".to_owned(), at(note.updated_at)),
    ]);
    Ok(props)
}

/// The columns [`chunk`] reads, for a chunk bound to `c`, without its
/// embedding. Add `c.embedding AS embedding` to read that too.
pub(crate) const CHUNK: &str = "c.id AS id, c.note_id AS note_id, c.ordinal AS ordinal, \
     c.text AS text, c.token_count AS token_count";

pub(crate) fn chunk(row: &Row, with_embedding: bool) -> Result<Chunk, StoreError> {
    let embedding = if with_embedding {
        get::<Option<Vec<f64>>>(row, "embedding")?.map(|values| {
            // Written from f32 values, so each one converts back exactly.
            #[expect(clippy::cast_possible_truncation, reason = "stored from f32")]
            values.into_iter().map(|value| value as f32).collect()
        })
    } else {
        None
    };
    Ok(Chunk {
        id: ChunkId::from_uuid(uuid(row, "id")?),
        note_id: NoteId::from_uuid(uuid(row, "note_id")?),
        ordinal: number(row, "ordinal")?,
        text: get(row, "text")?,
        token_count: number(row, "token_count")?,
        embedding,
    })
}

/// The note's provenance, for a search row that binds the note to `n`.
pub(crate) const HIT_PROVENANCE: &str = PROVENANCE;

pub(crate) fn hit_provenance(row: &Row) -> Result<Provenance, StoreError> {
    provenance(row)
}

/// The words full-text search matches: lowercased letters and digits, split
/// on everything else. Search indexes these, not the raw text, so the
/// database's own tokenizer can't decide that `v3.2` is one word.
pub(crate) fn terms(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A chunk's properties, for `CREATE (c:Chunk) SET c = $props`.
pub(crate) fn chunk_props(chunk: &Chunk) -> HashMap<String, BoltType> {
    let embedding = chunk
        .embedding
        .as_ref()
        .map(|values| values.iter().copied().map(f64::from).collect::<Vec<f64>>());
    let norm = embedding
        .as_ref()
        .map(|values| values.iter().map(|x| x * x).sum::<f64>().sqrt());
    HashMap::from([
        ("id".to_owned(), chunk.id.to_string().into()),
        ("note_id".to_owned(), chunk.note_id.to_string().into()),
        ("ordinal".to_owned(), i64::from(chunk.ordinal).into()),
        ("text".to_owned(), chunk.text.clone().into()),
        ("terms".to_owned(), terms(&chunk.text).join(" ").into()),
        (
            "token_count".to_owned(),
            i64::from(chunk.token_count).into(),
        ),
        ("embedding".to_owned(), embedding.into()),
        ("norm".to_owned(), norm.into()),
    ])
}

/// The columns [`task`] reads, for a task bound to `t`.
pub(crate) const TASK: &str = "t.id AS id, t.title AS title, t.detail AS detail, \
     t.status AS status, t.due_at AS due_at, t.created_at AS created_at";

pub(crate) fn task(row: &Row) -> Result<Task, StoreError> {
    Ok(Task {
        id: TaskId::from_uuid(uuid(row, "id")?),
        title: get(row, "title")?,
        detail: get(row, "detail")?,
        status: task_status_at(row, "status")?,
        due_at: optional_time(row, "due_at")?,
        created_at: time(row, "created_at")?,
    })
}

/// The columns [`working`] reads, for an entry bound to `w`.
pub(crate) const WORKING: &str =
    "w.scope AS scope, w.key AS key, w.value AS value, w.expires_at AS expires_at";

pub(crate) fn working(row: &Row) -> Result<WorkingEntry, StoreError> {
    Ok(WorkingEntry {
        scope: get(row, "scope")?,
        key: get(row, "key")?,
        value: json(row, "value")?,
        expires_at: optional_time(row, "expires_at")?,
    })
}

/// The columns [`job`] reads, for a job bound to `j`.
pub(crate) const JOB: &str = "j.id AS id, j.kind AS kind, j.payload AS payload, \
     j.priority AS priority, j.status AS status, j.requested_by AS requested_by, \
     j.run_at AS run_at, j.attempts AS attempts, j.max_attempts AS max_attempts, \
     j.lease_worker AS lease_worker, j.lease_until AS lease_until, \
     j.last_error AS last_error, j.result AS result, j.finished_at AS finished_at";

pub(crate) fn job(row: &Row) -> Result<Job, StoreError> {
    let worker: Option<String> = get(row, "lease_worker")?;
    let until = optional_time(row, "lease_until")?;
    Ok(Job {
        id: JobId::from_uuid(uuid(row, "id")?),
        kind: get(row, "kind")?,
        payload: json(row, "payload")?,
        priority: number(row, "priority")?,
        status: job_status_at(row, "status")?,
        requested_by: Actor::new(get::<String>(row, "requested_by")?),
        run_at: time(row, "run_at")?,
        attempts: number(row, "attempts")?,
        max_attempts: number(row, "max_attempts")?,
        lease: worker
            .zip(until)
            .map(|(worker, until)| Lease { worker, until }),
        last_error: get(row, "last_error")?,
        result: optional_json(row, "result")?,
        finished_at: optional_time(row, "finished_at")?,
    })
}

/// A new job's properties, for `CREATE (j:Job) SET j = $props`.
pub(crate) fn job_props(job: &Job) -> Result<HashMap<String, BoltType>, StoreError> {
    Ok(HashMap::from([
        ("id".to_owned(), job.id.to_string().into()),
        ("kind".to_owned(), job.kind.clone().into()),
        ("payload".to_owned(), json_text(&job.payload).into()),
        ("priority".to_owned(), i64::from(job.priority).into()),
        ("status".to_owned(), job_status_name(job.status).into()),
        ("requested_by".to_owned(), job.requested_by.as_str().into()),
        ("run_at".to_owned(), at(job.run_at)),
        ("attempts".to_owned(), int(job.attempts)?.into()),
        ("max_attempts".to_owned(), int(job.max_attempts)?.into()),
        (
            "lease_worker".to_owned(),
            job.lease.as_ref().map(|lease| lease.worker.clone()).into(),
        ),
        (
            "lease_until".to_owned(),
            maybe_at(job.lease.as_ref().map(|lease| lease.until)),
        ),
        ("last_error".to_owned(), job.last_error.clone().into()),
        (
            "result".to_owned(),
            job.result.as_ref().map(json_text).into(),
        ),
    ]))
}

/// The columns [`definition`] reads, for a definition bound to `d`.
pub(crate) const DEFINITION: &str = "d.kind AS kind, d.name AS name, d.body AS body, \
     d.source AS source, d.updated_at AS updated_at";

pub(crate) fn definition(row: &Row) -> Result<Definition, StoreError> {
    Ok(Definition {
        key: DefinitionKey {
            kind: definition_kind_at(row, "kind")?,
            name: get(row, "name")?,
        },
        body: json(row, "body")?,
        source: get(row, "source")?,
        updated_at: time(row, "updated_at")?,
    })
}

/// The columns [`record`] reads, for a record bound to `n`.
pub(crate) fn record_columns() -> String {
    format!("n.type AS type, n.id AS id, n.data AS data, {PROVENANCE}")
}

pub(crate) fn record(row: &Row) -> Result<Record, StoreError> {
    Ok(Record::new(
        get(row, "type")?,
        RecordId::from_uuid(uuid(row, "id")?),
        json(row, "data")?,
        provenance(row)?,
    ))
}

/// The property a filterable field is indexed under. The prefix keeps a
/// plugin's field from replacing one of the record's own properties.
pub(crate) fn field_property(field: &str) -> String {
    format!("field_{field}")
}

/// A filter's value as a parameter, of the type its field is stored as.
pub(crate) fn field_param(value: &FieldValue) -> BoltType {
    match value {
        FieldValue::Text(text) => text.clone().into(),
        FieldValue::Integer(number) => (*number).into(),
        FieldValue::Number(number) => (*number).into(),
        FieldValue::Bool(flag) => (*flag).into(),
        FieldValue::Timestamp(time) => at(*time),
    }
}
