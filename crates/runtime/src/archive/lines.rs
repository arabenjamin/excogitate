//! What an export holds: a manifest, and one type per file, each line one
//! object. Each has a published schema, so another tool can read an export
//! without this code, and an import checks every line against it.

use chrono::{DateTime, NaiveDate, Utc};
use protocol::{Actor, DataClass, Trust};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use store::{
    Chunk, ChunkId, Definition, DefinitionKey, DefinitionKind, Job, JobId, JobStatus, Lease, Link,
    Note, NoteId, Provenance, Record, RecordId, SpendDay, Task, TaskId, TaskStatus, WorkingEntry,
};
use uuid::Uuid;

/// The format's version. An import refuses one it doesn't know.
pub const FORMAT_VERSION: u32 = 1;

/// The longest embedding an export holds.
const MAX_EMBEDDING: usize = 8192;

/// What an export is, written last, so an export that stopped part way has
/// none and can't be imported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The format's version.
    pub format_version: u32,
    /// When it was made.
    pub exported_at: DateTime<Utc>,
    /// The runtime that made it, with its version.
    #[schemars(length(max = 128))]
    pub runtime: String,
    /// The store it came from: `duckdb` or `neo4j`.
    #[schemars(length(max = 32))]
    pub store: String,
    /// The width of the chunks' embeddings.
    pub embedding_dimension: u32,
    /// How many lines each file has.
    pub counts: Counts,
}

/// How many lines each file of an export has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    /// `notes.jsonl`.
    pub notes: u64,
    /// `chunks.jsonl`.
    pub chunks: u64,
    /// `tasks.jsonl`.
    pub tasks: u64,
    /// `working.jsonl`.
    pub working: u64,
    /// `jobs.jsonl`.
    pub jobs: u64,
    /// `definitions.jsonl`.
    pub definitions: u64,
    /// `records.jsonl`.
    pub records: u64,
    /// `links.jsonl`.
    pub links: u64,
    /// `spend.jsonl`.
    pub spend: u64,
}

/// Who may see something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Only providers that keep no logs.
    Private,
    /// Marked public.
    Public,
}

/// Whether something may be treated as instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Written by the owner or the runtime.
    Trusted,
    /// From outside, or written by a model.
    Untrusted,
}

/// Where something came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    /// Who may see it.
    pub data_class: Class,
    /// Whether it may be treated as instructions.
    pub trust: TrustLevel,
    /// Who stored it.
    #[schemars(length(max = 256))]
    pub stored_by: String,
    /// When.
    pub stored_at: DateTime<Utc>,
}

impl From<&Provenance> for Origin {
    fn from(provenance: &Provenance) -> Self {
        Self {
            data_class: match provenance.data_class {
                DataClass::Private => Class::Private,
                DataClass::Public => Class::Public,
            },
            trust: match provenance.trust {
                Trust::Trusted => TrustLevel::Trusted,
                Trust::Untrusted => TrustLevel::Untrusted,
            },
            stored_by: provenance.stored_by.as_str().to_owned(),
            stored_at: provenance.stored_at,
        }
    }
}

impl From<Origin> for Provenance {
    fn from(origin: Origin) -> Self {
        Self {
            data_class: match origin.data_class {
                Class::Private => DataClass::Private,
                Class::Public => DataClass::Public,
            },
            trust: match origin.trust {
                TrustLevel::Trusted => Trust::Trusted,
                TrustLevel::Untrusted => Trust::Untrusted,
            },
            stored_by: Actor::new(origin.stored_by),
            stored_at: origin.stored_at,
        }
    }
}

/// A note, without its chunks, which are in `chunks.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteLine {
    /// Its id.
    pub id: Uuid,
    /// Its title.
    #[schemars(length(max = 4096))]
    pub title: String,
    /// Its text.
    #[schemars(length(max = 1_000_000))]
    pub body: String,
    /// Its tags.
    #[schemars(length(max = 256), inner(length(max = 1024)))]
    pub tags: Vec<String>,
    /// Where it came from, if it was fetched.
    #[schemars(length(max = 8192))]
    pub source_url: Option<String>,
    /// A hash of its content.
    #[schemars(length(max = 256))]
    pub content_hash: String,
    /// Where it came from.
    pub origin: Origin,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

impl From<&Note> for NoteLine {
    fn from(note: &Note) -> Self {
        Self {
            id: note.id.as_uuid(),
            title: note.title.clone(),
            body: note.body.clone(),
            tags: note.tags.clone(),
            source_url: note.source_url.clone(),
            content_hash: note.content_hash.clone(),
            origin: (&note.provenance).into(),
            updated_at: note.updated_at,
        }
    }
}

impl From<NoteLine> for Note {
    fn from(line: NoteLine) -> Self {
        Self {
            id: NoteId::from_uuid(line.id),
            title: line.title,
            body: line.body,
            tags: line.tags,
            source_url: line.source_url,
            content_hash: line.content_hash,
            provenance: line.origin.into(),
            version: 1,
            updated_at: line.updated_at,
        }
    }
}

/// A chunk of a note. Chunks follow their notes' order, each note's in
/// order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChunkLine {
    /// Its id.
    pub id: Uuid,
    /// Its note.
    pub note_id: Uuid,
    /// Its place in the note, from 0.
    pub ordinal: u32,
    /// Its text.
    #[schemars(length(max = 1_000_000))]
    pub text: String,
    /// Its length in the embedder's tokens.
    pub token_count: u32,
    /// Its embedding, if it had one.
    #[schemars(length(max = MAX_EMBEDDING))]
    pub embedding: Option<Vec<f32>>,
}

impl From<&Chunk> for ChunkLine {
    fn from(chunk: &Chunk) -> Self {
        Self {
            id: chunk.id.as_uuid(),
            note_id: chunk.note_id.as_uuid(),
            ordinal: chunk.ordinal,
            text: chunk.text.clone(),
            token_count: chunk.token_count,
            embedding: chunk.embedding.clone(),
        }
    }
}

impl From<ChunkLine> for Chunk {
    fn from(line: ChunkLine) -> Self {
        Self {
            id: ChunkId::from_uuid(line.id),
            note_id: NoteId::from_uuid(line.note_id),
            ordinal: line.ordinal,
            text: line.text,
            token_count: line.token_count,
            embedding: line.embedding,
        }
    }
}

/// Where a task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Not started.
    Pending,
    /// Under way.
    Active,
    /// Finished.
    Done,
    /// Given up.
    Cancelled,
}

/// A task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskLine {
    /// Its id.
    pub id: Uuid,
    /// What to do.
    #[schemars(length(max = 4096))]
    pub title: String,
    /// More detail.
    #[schemars(length(max = 1_000_000))]
    pub detail: String,
    /// Where it stands.
    pub status: TaskState,
    /// When it's due.
    pub due_at: Option<DateTime<Utc>>,
    /// When it was made.
    pub created_at: DateTime<Utc>,
}

impl From<&Task> for TaskLine {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id.as_uuid(),
            title: task.title.clone(),
            detail: task.detail.clone(),
            status: match task.status {
                TaskStatus::Pending => TaskState::Pending,
                TaskStatus::Active => TaskState::Active,
                TaskStatus::Done => TaskState::Done,
                TaskStatus::Cancelled => TaskState::Cancelled,
            },
            due_at: task.due_at,
            created_at: task.created_at,
        }
    }
}

impl From<TaskLine> for Task {
    fn from(line: TaskLine) -> Self {
        Self {
            id: TaskId::from_uuid(line.id),
            title: line.title,
            detail: line.detail,
            status: match line.status {
                TaskState::Pending => TaskStatus::Pending,
                TaskState::Active => TaskStatus::Active,
                TaskState::Done => TaskStatus::Done,
                TaskState::Cancelled => TaskStatus::Cancelled,
            },
            due_at: line.due_at,
            created_at: line.created_at,
        }
    }
}

/// A working-memory entry that hadn't expired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkingLine {
    /// Its session or run.
    #[schemars(length(max = 1024))]
    pub scope: String,
    /// Its name there.
    #[schemars(length(max = 1024))]
    pub key: String,
    /// Its value.
    #[schemars(extend("x-extension" = "whatever was kept"))]
    pub value: Value,
    /// When it stops being readable.
    pub expires_at: Option<DateTime<Utc>>,
}

impl From<&WorkingEntry> for WorkingLine {
    fn from(entry: &WorkingEntry) -> Self {
        Self {
            scope: entry.scope.clone(),
            key: entry.key.clone(),
            value: entry.value.clone(),
            expires_at: entry.expires_at,
        }
    }
}

impl From<WorkingLine> for WorkingEntry {
    fn from(line: WorkingLine) -> Self {
        Self {
            scope: line.scope,
            key: line.key,
            value: line.value,
            expires_at: line.expires_at,
        }
    }
}

/// Where a job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting to run.
    Queued,
    /// Held by a worker.
    Claimed,
    /// Finished.
    Succeeded,
    /// Out of attempts.
    Dead,
}

/// A worker's hold on a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LeaseLine {
    /// The worker.
    #[schemars(length(max = 256))]
    pub worker: String,
    /// When it lapses.
    pub until: DateTime<Utc>,
}

/// A job, as it was: finished ones too, with their results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobLine {
    /// Its id.
    pub id: Uuid,
    /// Its kind.
    #[schemars(length(max = 256))]
    pub kind: String,
    /// Its input.
    #[schemars(extend("x-extension" = "the job kind's own payload"))]
    pub payload: Value,
    /// Higher runs first.
    pub priority: i32,
    /// Where it stands.
    pub status: JobState,
    /// Who asked for it.
    #[schemars(length(max = 256))]
    pub requested_by: String,
    /// The earliest it may run.
    pub run_at: DateTime<Utc>,
    /// How many times it was claimed.
    pub attempts: u32,
    /// How many claims it may have.
    pub max_attempts: u32,
    /// Its hold, while claimed.
    pub lease: Option<LeaseLine>,
    /// Why its last attempt failed.
    #[schemars(length(max = 65_536))]
    pub last_error: Option<String>,
    /// What it produced.
    #[schemars(extend("x-extension" = "the job kind's own result"))]
    pub result: Option<Value>,
    /// When it finished.
    pub finished_at: Option<DateTime<Utc>>,
}

const fn job_state(status: JobStatus) -> JobState {
    match status {
        JobStatus::Queued => JobState::Queued,
        JobStatus::Claimed => JobState::Claimed,
        JobStatus::Succeeded => JobState::Succeeded,
        JobStatus::Dead => JobState::Dead,
    }
}

impl From<&Job> for JobLine {
    fn from(job: &Job) -> Self {
        Self {
            id: job.id.as_uuid(),
            kind: job.kind.clone(),
            payload: job.payload.clone(),
            priority: job.priority,
            status: job_state(job.status),
            requested_by: job.requested_by.as_str().to_owned(),
            run_at: job.run_at,
            attempts: job.attempts,
            max_attempts: job.max_attempts,
            lease: job.lease.as_ref().map(|lease| LeaseLine {
                worker: lease.worker.clone(),
                until: lease.until,
            }),
            last_error: job.last_error.clone(),
            result: job.result.clone(),
            finished_at: job.finished_at,
        }
    }
}

impl From<JobLine> for Job {
    fn from(line: JobLine) -> Self {
        Self {
            id: JobId::from_uuid(line.id),
            kind: line.kind,
            payload: line.payload,
            priority: line.priority,
            status: match line.status {
                JobState::Queued => JobStatus::Queued,
                JobState::Claimed => JobStatus::Claimed,
                JobState::Succeeded => JobStatus::Succeeded,
                JobState::Dead => JobStatus::Dead,
            },
            requested_by: Actor::new(line.requested_by),
            run_at: line.run_at,
            attempts: line.attempts,
            max_attempts: line.max_attempts,
            lease: line.lease.map(|lease| Lease {
                worker: lease.worker,
                until: lease.until,
            }),
            last_error: line.last_error,
            result: line.result,
            finished_at: line.finished_at,
        }
    }
}

/// What a definition defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// An agent.
    Agent,
    /// A workflow.
    Workflow,
    /// A schedule.
    Schedule,
}

/// Each kind of definition, as the store has it.
pub const KINDS: [DefinitionKind; 3] = [
    DefinitionKind::Agent,
    DefinitionKind::Workflow,
    DefinitionKind::Schedule,
];

/// A definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DefinitionLine {
    /// What it defines.
    pub kind: Kind,
    /// Its name, which is also its file's.
    #[schemars(length(min = 1, max = 64), regex(pattern = r"^[a-z0-9_-]{1,64}$"))]
    pub name: String,
    /// The definition.
    #[schemars(extend("x-extension" = "checked against its kind's schema when it's loaded"))]
    pub body: Value,
    /// Where it came from, such as a file.
    #[schemars(length(max = 4096))]
    pub source: String,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

impl From<&Definition> for DefinitionLine {
    fn from(definition: &Definition) -> Self {
        Self {
            kind: match definition.key.kind {
                DefinitionKind::Agent => Kind::Agent,
                DefinitionKind::Workflow => Kind::Workflow,
                DefinitionKind::Schedule => Kind::Schedule,
            },
            name: definition.key.name.clone(),
            body: definition.body.clone(),
            source: definition.source.clone(),
            updated_at: definition.updated_at,
        }
    }
}

impl From<DefinitionLine> for Definition {
    fn from(line: DefinitionLine) -> Self {
        Self {
            key: DefinitionKey {
                kind: match line.kind {
                    Kind::Agent => DefinitionKind::Agent,
                    Kind::Workflow => DefinitionKind::Workflow,
                    Kind::Schedule => DefinitionKind::Schedule,
                },
                name: line.name,
            },
            body: line.body,
            source: line.source,
            updated_at: line.updated_at,
        }
    }
}

/// A plugin's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecordLine {
    /// Its type, as `plugin.type`.
    #[schemars(length(max = 128))]
    pub record_type: String,
    /// Its id.
    pub id: Uuid,
    /// Its data.
    #[schemars(extend("x-extension" = "checked against its type's schema on import"))]
    pub data: Value,
    /// Where it came from.
    pub origin: Origin,
}

impl From<&Record> for RecordLine {
    fn from(record: &Record) -> Self {
        Self {
            record_type: record.record_type.clone(),
            id: record.id.as_uuid(),
            data: record.data.clone(),
            origin: (&record.provenance).into(),
        }
    }
}

impl From<RecordLine> for Record {
    fn from(line: RecordLine) -> Self {
        Self::new(
            line.record_type,
            RecordId::from_uuid(line.id),
            line.data,
            line.origin.into(),
        )
    }
}

/// A link between two records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkLine {
    /// The relation, as `plugin.relation`.
    #[schemars(length(max = 128))]
    pub relation: String,
    /// The record it links from.
    pub from: Uuid,
    /// The record it links to.
    pub to: Uuid,
}

impl From<&Link> for LinkLine {
    fn from(link: &Link) -> Self {
        Self {
            relation: link.relation.clone(),
            from: link.from.as_uuid(),
            to: link.to.as_uuid(),
        }
    }
}

/// What a provider spent on one UTC day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpendLine {
    /// The provider.
    #[schemars(length(max = 256))]
    pub provider: String,
    /// The day.
    pub day: NaiveDate,
    /// What it spent or reserved, in millionths of a dollar.
    pub usd_micros: u64,
}

impl From<&SpendDay> for SpendLine {
    fn from(day: &SpendDay) -> Self {
        Self {
            provider: day.provider.clone(),
            day: day.day,
            usd_micros: day.usd_micros,
        }
    }
}
