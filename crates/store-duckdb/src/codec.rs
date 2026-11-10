//! Between Rust types and columns. Everything read back is checked, so a
//! bad row is an error rather than a guess.

use std::fmt::Display;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use duckdb::Row;
use duckdb::types::{Type, Value as Sql};
use protocol::{Actor, DataClass, Trust};
use serde_json::Value;
use store::{
    Chunk, ChunkId, Definition, DefinitionKey, DefinitionKind, Job, JobId, JobStatus, Lease, Note,
    NoteId, Provenance, Record, RecordId, StoreError, Task, TaskId, TaskStatus, WorkingEntry,
};
use uuid::Uuid;

pub(crate) fn backend(error: impl Display) -> StoreError {
    StoreError::Backend(schema::bounded(&error.to_string()))
}

/// A column that doesn't hold what it should.
fn bad_column(index: usize, problem: impl Into<String>) -> duckdb::Error {
    let message: Box<dyn std::error::Error + Send + Sync> = problem.into().into();
    duckdb::Error::FromSqlConversionFailure(index, Type::Text, message)
}

fn uuid_at(row: &Row<'_>, index: usize) -> duckdb::Result<Uuid> {
    let text: String = row.get(index)?;
    Uuid::from_str(&text).map_err(|error| bad_column(index, error.to_string()))
}

fn json_at(row: &Row<'_>, index: usize) -> duckdb::Result<Value> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|error| bad_column(index, error.to_string()))
}

fn number_at<N: TryFrom<i64>>(row: &Row<'_>, index: usize) -> duckdb::Result<N> {
    let raw: i64 = row.get(index)?;
    N::try_from(raw).map_err(|_| bad_column(index, "number out of range"))
}

macro_rules! text_enum {
    ($type:ty, $to:ident, $from:ident, $($variant:path => $name:literal),+ $(,)?) => {
        pub(crate) const fn $to(value: $type) -> &'static str {
            match value { $($variant => $name),+ }
        }

        fn $from(row: &Row<'_>, index: usize) -> duckdb::Result<$type> {
            let text: String = row.get(index)?;
            match text.as_str() {
                $($name => Ok($variant),)+
                other => Err(bad_column(index, format!("unknown value {other}"))),
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
    DefinitionKind::Agent => "agent", DefinitionKind::Workflow => "workflow");

/// A vector as a bind parameter, for `?::FLOAT[n]`.
pub(crate) fn vector_param(vector: &[f32]) -> Sql {
    Sql::List(vector.iter().copied().map(Sql::Float).collect())
}

fn vector_at(row: &Row<'_>, index: usize) -> duckdb::Result<Option<Vec<f32>>> {
    let items = match row.get::<_, Sql>(index)? {
        Sql::Null => return Ok(None),
        Sql::Array(items) | Sql::List(items) => items,
        _ => return Err(bad_column(index, "not a vector")),
    };
    items
        .into_iter()
        .map(|item| match item {
            Sql::Float(value) => Ok(value),
            _ => Err(bad_column(index, "not a float")),
        })
        .collect::<duckdb::Result<Vec<f32>>>()
        .map(Some)
}

fn provenance_at(row: &Row<'_>, first: usize) -> duckdb::Result<Provenance> {
    Ok(Provenance {
        data_class: data_class_at(row, first)?,
        trust: trust_at(row, first + 1)?,
        stored_by: Actor::new(row.get::<_, String>(first + 2)?),
        stored_at: row.get::<_, DateTime<Utc>>(first + 3)?,
    })
}

/// The columns [`note_at`] reads.
pub(crate) const NOTE_COLUMNS: &str = "id, title, body, tags, source_url, content_hash, \
     data_class, trust, stored_by, stored_at, version, updated_at";

pub(crate) fn note_at(row: &Row<'_>) -> duckdb::Result<Note> {
    let tags: String = row.get(3)?;
    Ok(Note {
        id: NoteId::from_uuid(uuid_at(row, 0)?),
        title: row.get(1)?,
        body: row.get(2)?,
        tags: serde_json::from_str(&tags).map_err(|error| bad_column(3, error.to_string()))?,
        source_url: row.get(4)?,
        content_hash: row.get(5)?,
        provenance: provenance_at(row, 6)?,
        version: number_at(row, 10)?,
        updated_at: row.get(11)?,
    })
}

/// The columns [`chunk_at`] reads, for a query aliasing `chunks c`, and
/// `embedding` as the vector or NULL.
pub(crate) const CHUNK_COLUMNS: &str = "c.id, c.note_id, c.ordinal, c.text, c.token_count";

pub(crate) fn chunk_at(row: &Row<'_>, embedding: Option<usize>) -> duckdb::Result<Chunk> {
    Ok(Chunk {
        id: ChunkId::from_uuid(uuid_at(row, 0)?),
        note_id: NoteId::from_uuid(uuid_at(row, 1)?),
        ordinal: number_at(row, 2)?,
        text: row.get(3)?,
        token_count: number_at(row, 4)?,
        embedding: match embedding {
            Some(index) => vector_at(row, index)?,
            None => None,
        },
    })
}

/// A row of [`CHUNK_COLUMNS`], then the note's four provenance columns.
pub(crate) fn hit_provenance_at(row: &Row<'_>) -> duckdb::Result<Provenance> {
    provenance_at(row, 5)
}

pub(crate) const TASK_COLUMNS: &str = "id, title, detail, status, due_at, created_at";

pub(crate) fn task_at(row: &Row<'_>) -> duckdb::Result<Task> {
    Ok(Task {
        id: TaskId::from_uuid(uuid_at(row, 0)?),
        title: row.get(1)?,
        detail: row.get(2)?,
        status: task_status_at(row, 3)?,
        due_at: row.get(4)?,
        created_at: row.get(5)?,
    })
}

pub(crate) const WORKING_COLUMNS: &str = "scope, key, value, expires_at";

pub(crate) fn working_at(row: &Row<'_>) -> duckdb::Result<WorkingEntry> {
    Ok(WorkingEntry {
        scope: row.get(0)?,
        key: row.get(1)?,
        value: json_at(row, 2)?,
        expires_at: row.get(3)?,
    })
}

pub(crate) const JOB_COLUMNS: &str = "id, kind, payload, priority, status, requested_by, run_at, \
     attempts, max_attempts, lease_worker, lease_until, last_error";

pub(crate) fn job_at(row: &Row<'_>) -> duckdb::Result<Job> {
    let worker: Option<String> = row.get(9)?;
    let until: Option<DateTime<Utc>> = row.get(10)?;
    Ok(Job {
        id: JobId::from_uuid(uuid_at(row, 0)?),
        kind: row.get(1)?,
        payload: json_at(row, 2)?,
        priority: row.get(3)?,
        status: job_status_at(row, 4)?,
        requested_by: Actor::new(row.get::<_, String>(5)?),
        run_at: row.get(6)?,
        attempts: number_at(row, 7)?,
        max_attempts: number_at(row, 8)?,
        lease: worker
            .zip(until)
            .map(|(worker, until)| Lease { worker, until }),
        last_error: row.get(11)?,
    })
}

pub(crate) const DEFINITION_COLUMNS: &str = "kind, name, body, source, updated_at";

pub(crate) fn definition_at(row: &Row<'_>) -> duckdb::Result<Definition> {
    Ok(Definition {
        key: DefinitionKey {
            kind: definition_kind_at(row, 0)?,
            name: row.get(1)?,
        },
        body: json_at(row, 2)?,
        source: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

/// The columns [`record_at`] reads, for a query aliasing `records r`.
pub(crate) const RECORD_COLUMNS: &str =
    "r.record_type, r.id, r.data, r.data_class, r.trust, r.stored_by, r.stored_at";

pub(crate) fn record_at(row: &Row<'_>) -> duckdb::Result<Record> {
    Ok(Record {
        record_type: row.get(0)?,
        id: RecordId::from_uuid(uuid_at(row, 1)?),
        data: json_at(row, 2)?,
        provenance: provenance_at(row, 3)?,
    })
}
