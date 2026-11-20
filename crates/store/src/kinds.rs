//! The kinds the core uses: notes and retrieval, tasks and working memory,
//! jobs, and definitions.

use chrono::{DateTime, Utc};
use protocol::{Actor, DataClass, Reader, Trust};
use serde_json::Value;

use crate::{ChunkId, JobId, NoteId, TaskId};

/// Where content came from. The runtime sets all of it, never the model.
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
    /// Who it's private to, if anyone: a note stored by a private run is
    /// private to whoever asked for it. Only they, and a read with no reader,
    /// see it.
    pub private_to: Option<Reader>,
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

/// When notes were stored, for [`NoteReader::notes_stored`]: from `since`,
/// up to but not including `until`, or with no end.
///
/// [`NoteReader::notes_stored`]: crate::NoteReader::notes_stored
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredWindow {
    /// The earliest `stored_at` kept.
    pub since: DateTime<Utc>,
    /// The first `stored_at` past the window, if it ends.
    pub until: Option<DateTime<Utc>>,
}

impl StoredWindow {
    /// Whether a note stored at `at` is in it.
    #[must_use]
    pub fn holds(&self, at: DateTime<Utc>) -> bool {
        at >= self.since && self.until.is_none_or(|until| at < until)
    }
}

/// A page of the notes a window holds, and how many it holds in all.
#[derive(Debug, Clone, PartialEq)]
pub struct NotesStored {
    /// The notes, the latest stored first, then by id.
    pub notes: crate::Page<Note>,
    /// How many notes the window holds, of the classes asked for.
    pub total: u64,
}

/// What a read may return: which privacy classes, and, for private notes,
/// who's reading. A note private to one reader is returned only to that
/// reader, or to a read with no reader (the owner's, and the runtime's own).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassFilter {
    public_only: bool,
    reader: Option<Reader>,
}

impl ClassFilter {
    /// Every class, and every note whoever it's private to.
    pub const ANY: Self = Self {
        public_only: false,
        reader: None,
    };
    /// Only what's marked public, and every note whoever it's private to.
    pub const PUBLIC_ONLY: Self = Self {
        public_only: true,
        reader: None,
    };

    /// The same filter, reading as `reader`: someone else's private notes are
    /// left out. None reads everything.
    #[must_use]
    pub const fn reading_as(mut self, reader: Option<Reader>) -> Self {
        self.reader = reader;
        self
    }

    /// True if only public notes may be returned.
    #[must_use]
    pub const fn public_only(self) -> bool {
        self.public_only
    }

    /// Who's reading, if a reader was given.
    #[must_use]
    pub const fn reader(self) -> Option<Reader> {
        self.reader
    }

    /// Whether a note of `data_class`, private to `private_to` if anyone,
    /// may be returned.
    #[must_use]
    pub fn shows(self, data_class: DataClass, private_to: Option<Reader>) -> bool {
        let class_ok = !self.public_only || data_class == DataClass::Public;
        let reader_ok = match (self.reader, private_to) {
            (Some(reader), Some(owner)) => reader == owner,
            _ => true,
        };
        class_ok && reader_ok
    }
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
    /// When it succeeded or went dead, by the store's clock. Set by the
    /// store; whatever a new job carries here is ignored.
    pub finished_at: Option<DateTime<Utc>>,
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
