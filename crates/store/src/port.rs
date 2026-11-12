//! The traits a store implements. Reads are split by kind; all writes go
//! through [`Store::apply`].

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

use crate::{
    Chunk, ChunkHit, ClassFilter, Definition, DefinitionKey, DefinitionKind, Direction, Job, JobId,
    JobStatus, Note, NoteId, Page, PageRequest, Record, RecordDecls, RecordId, RecordQuery,
    StoreError, Task, TaskId, TaskQuery, UnitOfWork, WorkingEntry,
};

/// Reads notes, and searches their chunks. Fusing the two searches happens
/// above the port, so it's the same on every store.
#[async_trait]
pub trait NoteReader {
    /// The width of the embeddings this store holds.
    fn embedding_dimension(&self) -> usize;

    /// A note, if it exists.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn get_note(&self, id: NoteId) -> Result<Option<Note>, StoreError>;

    /// A note with this content hash, if one exists.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn find_note_by_hash(&self, hash: &str) -> Result<Option<Note>, StoreError>;

    /// A note's chunks, in order.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn list_chunks(&self, note_id: NoteId) -> Result<Vec<Chunk>, StoreError>;

    /// Full-text search, best first. Case doesn't matter, and digits count
    /// (`2026` and `v3` must be findable).
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn search_text(
        &self,
        query: &str,
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<Page<ChunkHit>, StoreError>;

    /// Nearest chunks by embedding, best first. Chunks without an embedding
    /// are left out.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if `vector` has the wrong width.
    /// [`StoreError::Backend`] if the store fails.
    async fn search_vector(
        &self,
        vector: &[f32],
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<Page<ChunkHit>, StoreError>;
}

/// Reads tasks and working memory.
#[async_trait]
pub trait TaskReader {
    /// A task, if it exists.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn get_task(&self, id: TaskId) -> Result<Option<Task>, StoreError>;

    /// Tasks, oldest first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn list_tasks(
        &self,
        query: TaskQuery,
        page: PageRequest,
    ) -> Result<Page<Task>, StoreError>;

    /// A working-memory entry, unless it doesn't exist or expired by `now`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn get_working(
        &self,
        scope: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<WorkingEntry>, StoreError>;
}

/// The durable job queue. Time is passed in, so behavior can be tested.
#[async_trait]
pub trait JobQueue {
    /// A job, if it exists.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn get_job(&self, id: JobId) -> Result<Option<Job>, StoreError>;

    /// Lists jobs, newest `run_at` first, optionally with one status.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn list_jobs(
        &self,
        status: Option<JobStatus>,
        page: PageRequest,
    ) -> Result<Page<Job>, StoreError>;

    /// Atomically claims the next runnable job of one of `kinds`: highest
    /// priority, then earliest `run_at`, then lowest id. A job of another
    /// kind is left for a worker that can run it, and no kinds claims
    /// nothing. A job whose lease has lapsed is runnable again, or dead if
    /// it's out of attempts, whatever its kind. Each claim counts as an
    /// attempt.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if `lease_until` isn't after `now`.
    /// [`StoreError::Backend`] if the store fails.
    async fn claim_job(
        &self,
        worker: &str,
        kinds: &[String],
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError>;

    /// Marks a claimed job finished, and keeps what it produced. The worker
    /// named in its lease may finish it while it's still claimed, even past
    /// the lease's end: until another claim takes it again or, on its last
    /// attempt, marks it dead.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] if there's no such job.
    /// [`StoreError::Conflict`] if `worker` doesn't hold it.
    async fn complete_job(&self, id: JobId, worker: &str, result: Value) -> Result<(), StoreError>;

    /// Records a failed attempt, and what it left behind, if anything. The
    /// job runs again at `retry_at`, or goes dead if it's out of attempts.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] if there's no such job.
    /// [`StoreError::Conflict`] if `worker` doesn't hold it.
    async fn fail_job(
        &self,
        id: JobId,
        worker: &str,
        error: &str,
        result: Option<Value>,
        retry_at: DateTime<Utc>,
    ) -> Result<(), StoreError>;
}

/// Reads agent and workflow definitions.
#[async_trait]
pub trait DefinitionReader {
    /// A definition, if it exists.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn get_definition(&self, key: &DefinitionKey) -> Result<Option<Definition>, StoreError>;

    /// Definitions of one kind, by name.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn list_definitions(
        &self,
        kind: DefinitionKind,
        page: PageRequest,
    ) -> Result<Page<Definition>, StoreError>;
}

/// Reads plugin records, with typed filters only.
#[async_trait]
pub trait RecordReader {
    /// Declares record types and relations, all of them or none.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if a declaration is malformed or its schema
    /// isn't strict. [`StoreError::Conflict`] if a name is taken.
    async fn register_records(&self, decls: &RecordDecls) -> Result<(), StoreError>;

    /// A record, if it exists.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if the type is undeclared.
    /// [`StoreError::Backend`] if the store fails.
    async fn get_record(
        &self,
        record_type: &str,
        id: RecordId,
    ) -> Result<Option<Record>, StoreError>;

    /// Records meeting every filter, in id order.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if the type is undeclared, or a filter names
    /// an undeclared field or has a value of the wrong type.
    /// [`StoreError::Backend`] if the store fails.
    async fn query_records(&self, query: RecordQuery) -> Result<Page<Record>, StoreError>;

    /// The records a relation links to or from `id`, in id order.
    ///
    /// # Errors
    ///
    /// [`StoreError::Invalid`] if the relation is undeclared.
    /// [`StoreError::Backend`] if the store fails.
    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: Direction,
        page: PageRequest,
    ) -> Result<Page<Record>, StoreError>;
}

/// What each provider has spent per UTC day, in millionths of a dollar. It
/// has to survive a restart, or a restart would reset a daily cap.
#[async_trait]
pub trait SpendLog {
    /// Adds `amount` to the day if the total stays within `cap`. Returns
    /// false and changes nothing if it wouldn't. Atomic, so calls made at the
    /// same time can't overrun the cap together.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn reserve_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        amount: u64,
        cap: u64,
    ) -> Result<bool, StoreError>;

    /// Replaces a reservation with what the call really cost. The total
    /// never goes below zero.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn settle_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        reserved: u64,
        actual: u64,
    ) -> Result<(), StoreError>;

    /// What the provider has spent or reserved that day.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if the store fails.
    async fn spent_on(&self, provider: &str, day: NaiveDate) -> Result<u64, StoreError>;
}

/// A complete store. The `store-conformance` crate defines what it must do.
#[async_trait]
pub trait Store:
    NoteReader
    + TaskReader
    + JobQueue
    + DefinitionReader
    + RecordReader
    + SpendLog
    + Send
    + Sync
    + std::fmt::Debug
{
    /// Applies every write in `unit` in order, or none of them. Readers see
    /// all of it or none of it.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`], [`StoreError::Conflict`] or
    /// [`StoreError::Invalid`] from the first write that can't be made, in
    /// which case nothing changed. [`StoreError::Backend`] if the store fails.
    async fn apply(&self, unit: UnitOfWork) -> Result<(), StoreError>;
}
