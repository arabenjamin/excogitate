//! The kinds the core uses: notes and retrieval, tasks and working memory,
//! jobs, and definitions.

use chrono::{DateTime, Utc};
use protocol::{Actor, DataClass, Trust};
use serde_json::Value;

use crate::{ChunkId, JobId, NoteId, TaskId};

/// Where content came from. The brain sets all of it, never the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// Who may see it.
    pub data_class: DataClass,
    /// Whether it may be treated as instructions.
    pub trust: Trust,
    /// Who stored it.
    pub stored_by: Actor,
    /// When it was stored.
    pub stored_at: DateTime<Utc>,
}

/// A piece of long-term memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Its id.
    pub id: NoteId,
    /// A short title.
    pub title: String,
    /// The full text.
    pub body: String,
    /// Tags.
    pub tags: Vec<String>,
    /// The URL it came from, when it was fetched.
    pub source_url: Option<String>,
    /// A hash of the content, for finding an existing copy.
    pub content_hash: String,
    /// Where it came from.
    pub provenance: Provenance,
    /// Set by the store: 1 when created, and one more on each update.
    pub version: u64,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}

/// A retrievable piece of a note.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// Its id.
    pub id: ChunkId,
    /// The note it belongs to.
    pub note_id: NoteId,
    /// Its position in the note, from 0.
    pub ordinal: u32,
    /// Its text.
    pub text: String,
    /// Its length in the embedder's tokens.
    pub token_count: u32,
    /// Its embedding, if an embedder was available.
    pub embedding: Option<Vec<f32>>,
}

/// A search result. It carries the note's provenance, so a label such as
/// untrusted still applies once the text is back in a prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkHit {
    /// The chunk found, without its embedding.
    pub chunk: Chunk,
    /// Where its note came from.
    pub provenance: Provenance,
    /// How well it matched; higher is better. Comparable only within one
    /// search, so fusing results works on rank.
    pub score: f32,
}

/// Which privacy classes a search may return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassFilter {
    /// Everything.
    Any,
    /// Only what's marked public.
    PublicOnly,
}

/// Where a task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskStatus {
    /// Not started.
    Pending,
    /// Under way.
    Active,
    /// Finished.
    Done,
    /// Given up.
    Cancelled,
}

/// Something to be done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// Its id.
    pub id: TaskId,
    /// What to do.
    pub title: String,
    /// More detail.
    pub detail: String,
    /// Where it stands.
    pub status: TaskStatus,
    /// When it's due, if it is.
    pub due_at: Option<DateTime<Utc>>,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

/// Which tasks to list. Empty fields don't filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TaskQuery {
    /// Only tasks with this status.
    pub status: Option<TaskStatus>,
    /// Only tasks due at or before this time.
    pub due_by: Option<DateTime<Utc>>,
}

/// A short-lived value, held for a session or a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingEntry {
    /// The session or run it belongs to.
    pub scope: String,
    /// Its name in that scope.
    pub key: String,
    /// The value.
    pub value: Value,
    /// When it stops being readable, if ever.
    pub expires_at: Option<DateTime<Utc>>,
}

/// Where a job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobStatus {
    /// Waiting to run.
    Queued,
    /// Held by a worker.
    Claimed,
    /// Finished.
    Succeeded,
    /// Out of attempts. It stays readable, so the failure is visible.
    Dead,
}

/// A worker's hold on a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The worker holding it.
    pub worker: String,
    /// When it lapses and the job can be claimed again.
    pub until: DateTime<Utc>,
}

/// A unit of queued work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Its id.
    pub id: JobId,
    /// What kind of work it is.
    pub kind: String,
    /// Its input.
    pub payload: Value,
    /// Higher runs first.
    pub priority: i32,
    /// Where it stands.
    pub status: JobStatus,
    /// Who asked for it, from the first commit, so users and roles need no
    /// migration later.
    pub requested_by: Actor,
    /// The earliest it may run.
    pub run_at: DateTime<Utc>,
    /// How many times it has been claimed.
    pub attempts: u32,
    /// How many claims it may have.
    pub max_attempts: u32,
    /// The current hold, while claimed.
    pub lease: Option<Lease>,
    /// Why the last attempt failed.
    pub last_error: Option<String>,
    /// What it produced, or what its last failed attempt left behind.
    pub result: Option<Value>,
}

/// What a definition defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DefinitionKind {
    /// An agent.
    Agent,
    /// A workflow.
    Workflow,
    /// When to enqueue a job.
    Schedule,
}

/// A definition's one home: there's one per kind and name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DefinitionKey {
    /// What it defines.
    pub kind: DefinitionKind,
    /// Its name.
    pub name: String,
}

/// An agent or workflow definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// Which definition this is.
    pub key: DefinitionKey,
    /// The definition, already validated against its schema.
    pub body: Value,
    /// Where it came from, such as a file path.
    pub source: String,
    /// When it last changed.
    pub updated_at: DateTime<Utc>,
}
