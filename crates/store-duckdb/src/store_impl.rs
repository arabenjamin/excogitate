//! The store itself: async on the outside, one writer thread and short-lived
//! reader connections on the inside.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use duckdb::{Connection, params};
use serde_json::Value;
use store::{
    Chunk, ChunkHit, ClassFilter, Definition, DefinitionKey, DefinitionKind, DefinitionReader,
    Direction, Job, JobId, JobQueue, JobStatus, Note, NoteId, NoteReader, Page, PageRequest,
    Record, RecordCatalog, RecordDecls, RecordId, RecordQuery, RecordReader, SpendLog, Store,
    StoreError, Task, TaskId, TaskQuery, TaskReader, UnitOfWork, WorkingEntry,
};
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::codec::{
    DEFINITION_COLUMNS, JOB_COLUMNS, NOTE_COLUMNS, TASK_COLUMNS, WORKING_COLUMNS, backend,
    definition_at, definition_kind_name, job_at, job_status_name, note_at, task_at,
    task_status_name, vector_param, working_at,
};
use crate::read::{self, optional, paged};
use crate::schema_sql;
use crate::writer::{self, Command};

/// How many writes can wait for the writer.
const WRITE_QUEUE: usize = 1024;
/// How many reads run at once. `DuckDB` parallelizes inside each one.
const MAX_READERS: usize = 16;

/// How to open a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuckDbConfig {
    /// The database file. `None` keeps it in memory, for tests.
    pub path: Option<PathBuf>,
    /// The directory holding the extensions. Nothing is ever downloaded.
    pub extension_dir: PathBuf,
    /// The width of the vectors. Fixed for the life of the file.
    pub embedding_dimension: usize,
}

/// What `DuckDB` says when another process holds the file.
const LOCK_CONFLICT: &str = "Could not set lock on file";

/// A store could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// `DuckDB` failed.
    #[error("couldn't open the database: {0}")]
    Database(String),
    /// Another process has it open, and only one can.
    #[error(
        "another process has the store open, and only one can. If that's the server, \
         use the `connect` command to reach it"
    )]
    InUse,
    /// A required extension isn't where it should be.
    #[error("{0}")]
    Extension(String),
    /// The file can't be used by this build or this configuration.
    #[error("{0}")]
    Incompatible(String),
    /// The configuration makes no sense.
    #[error("{0}")]
    Invalid(String),
}

/// The default store.
#[derive(Debug)]
pub struct DuckDbStore {
    commands: mpsc::Sender<Command>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    root: Arc<Mutex<Connection>>,
    readers: Arc<Semaphore>,
    catalog: Arc<RwLock<RecordCatalog>>,
    dimension: usize,
}

/// The writer finishes its last checkpoint before the store is gone. A
/// writer left running can outlive its process, which then crashes in
/// `DuckDB`'s teardown, or can still be writing when the file is opened again,
/// which corrupts it.
impl Drop for DuckDbStore {
    fn drop(&mut self) {
        let handle = self
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        // The writer stops when every sender is gone, and this is the only one.
        let (stand_in, _) = mpsc::channel(1);
        drop(std::mem::replace(&mut self.commands, stand_in));
        if let Some(handle) = handle {
            // A panicked writer has nothing more to clean up.
            let _ = handle.join();
        }
    }
}

impl DuckDbStore {
    /// Opens or creates the store.
    ///
    /// # Errors
    ///
    /// [`OpenError`] if the file can't be opened, the extension is missing,
    /// or the file doesn't match this build or configuration.
    pub fn open(config: &DuckDbConfig) -> Result<Self, OpenError> {
        let dimension = config.embedding_dimension;
        if !(1..=65535).contains(&dimension) {
            return Err(OpenError::Invalid(format!(
                "embedding dimension must be 1 to 65535, got {dimension}"
            )));
        }
        let database = |error: duckdb::Error| {
            let message = error.to_string();
            if message.contains(LOCK_CONFLICT) {
                OpenError::InUse
            } else {
                OpenError::Database(message)
            }
        };
        let root = match &config.path {
            Some(path) => Connection::open(path),
            None => Connection::open_in_memory(),
        }
        .map_err(database)?;
        schema_sql::prepare(&root, &config.extension_dir, dimension)?;

        let writer_connection = root.try_clone().map_err(database)?;
        let catalog = Arc::new(RwLock::new(RecordCatalog::new()));
        let (commands, receiver) = mpsc::channel(WRITE_QUEUE);
        let writer_catalog = Arc::clone(&catalog);
        let writer = std::thread::Builder::new()
            .name("store-writer".to_owned())
            .spawn(move || {
                writer::run(&writer_connection, receiver, &writer_catalog, dimension);
            })
            .map_err(|error| OpenError::Database(error.to_string()))?;
        Ok(Self {
            commands,
            writer: Mutex::new(Some(writer)),
            root: Arc::new(Mutex::new(root)),
            readers: Arc::new(Semaphore::new(MAX_READERS)),
            catalog,
            dimension,
        })
    }

    /// Stops the writer, after a checkpoint, and waits for it. Dropping the
    /// store does the same; this says so where it happens.
    pub fn close(self) {
        drop(self);
    }

    async fn send<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, StoreError>>) -> Command,
    ) -> Result<T, StoreError> {
        let stopped = || StoreError::Backend("the store's writer has stopped".to_owned());
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| stopped())?;
        answer.await.map_err(|_| stopped())?
    }

    async fn read<T: Send + 'static>(
        &self,
        query: impl FnOnce(&Connection) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let _permit = self.readers.acquire().await.map_err(backend)?;
        let root = Arc::clone(&self.root);
        tokio::task::spawn_blocking(move || {
            let connection = root
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .try_clone()
                .map_err(backend)?;
            query(&connection)
        })
        .await
        .map_err(backend)?
    }

    fn catalog_snapshot(&self) -> Arc<RwLock<RecordCatalog>> {
        Arc::clone(&self.catalog)
    }

    /// Forces the write-ahead log into the file.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if it can't.
    pub async fn checkpoint(&self) -> Result<(), StoreError> {
        self.send(Command::Checkpoint).await
    }

    /// Writes the whole database to `directory` as Parquet. Do this before
    /// every `DuckDB` upgrade: a newer file can't be opened by an older build.
    ///
    /// # Errors
    ///
    /// [`StoreError::Backend`] if it can't.
    pub async fn export(&self, directory: &Path) -> Result<(), StoreError> {
        let directory = directory.to_string_lossy().into_owned();
        self.send(|reply| Command::Export(directory, reply)).await
    }
}

#[async_trait]
impl NoteReader for DuckDbStore {
    fn embedding_dimension(&self) -> usize {
        self.dimension
    }

    async fn get_note(&self, id: NoteId) -> Result<Option<Note>, StoreError> {
        self.read(move |connection| {
            optional(connection.query_row(
                &format!("SELECT {NOTE_COLUMNS} FROM notes WHERE id = ?"),
                params![id.to_string()],
                note_at,
            ))
        })
        .await
    }

    async fn find_note_by_hash(&self, hash: &str) -> Result<Option<Note>, StoreError> {
        let hash = hash.to_owned();
        self.read(move |connection| {
            optional(connection.query_row(
                &format!(
                    "SELECT {NOTE_COLUMNS} FROM notes WHERE content_hash = ? ORDER BY id LIMIT 1"
                ),
                params![hash],
                note_at,
            ))
        })
        .await
    }

    async fn list_chunks(&self, note_id: NoteId) -> Result<Vec<Chunk>, StoreError> {
        self.read(move |connection| read::chunks_of(connection, &note_id.to_string()))
            .await
    }

    async fn search_text(
        &self,
        query: &str,
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<Page<ChunkHit>, StoreError> {
        let query = query.to_owned();
        self.read(move |connection| read::search_text(connection, &query, classes, page))
            .await
    }

    async fn search_vector(
        &self,
        vector: &[f32],
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<Page<ChunkHit>, StoreError> {
        if vector.len() != self.dimension {
            return Err(StoreError::Invalid(format!(
                "query vector must have width {}, got {}",
                self.dimension,
                vector.len()
            )));
        }
        let (bound, dimension) = (vector_param(vector), self.dimension);
        self.read(move |connection| {
            read::search_vector(connection, &bound, dimension, classes, page)
        })
        .await
    }
}

#[async_trait]
impl TaskReader for DuckDbStore {
    async fn get_task(&self, id: TaskId) -> Result<Option<Task>, StoreError> {
        self.read(move |connection| {
            optional(connection.query_row(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?"),
                params![id.to_string()],
                task_at,
            ))
        })
        .await
    }

    async fn list_tasks(
        &self,
        query: TaskQuery,
        page: PageRequest,
    ) -> Result<Page<Task>, StoreError> {
        self.read(move |connection| {
            let status = query.status.map(task_status_name);
            let sql = format!(
                "SELECT {TASK_COLUMNS} FROM tasks
                 WHERE (?::VARCHAR IS NULL OR status = ?)
                   AND (?::TIMESTAMP IS NULL OR due_at <= ?)
                 ORDER BY created_at, id"
            );
            paged(
                connection,
                &sql,
                params![status, status, query.due_by, query.due_by],
                page,
                task_at,
            )
        })
        .await
    }

    async fn get_working(
        &self,
        scope: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<WorkingEntry>, StoreError> {
        let (scope, key) = (scope.to_owned(), key.to_owned());
        self.read(move |connection| {
            optional(connection.query_row(
                &format!(
                    "SELECT {WORKING_COLUMNS} FROM working
                     WHERE scope = ? AND key = ? AND (expires_at IS NULL OR expires_at > ?)"
                ),
                params![scope, key, now],
                working_at,
            ))
        })
        .await
    }
}

#[async_trait]
impl JobQueue for DuckDbStore {
    async fn get_job(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        self.read(move |connection| {
            optional(connection.query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?"),
                params![id.to_string()],
                job_at,
            ))
        })
        .await
    }

    async fn list_jobs(
        &self,
        status: Option<JobStatus>,
        page: PageRequest,
    ) -> Result<Page<Job>, StoreError> {
        self.read(move |connection| {
            let status = status.map(job_status_name);
            let sql = format!(
                "SELECT {JOB_COLUMNS} FROM jobs
                 WHERE (?::VARCHAR IS NULL OR status = ?)
                 ORDER BY run_at DESC, id"
            );
            paged(connection, &sql, params![status, status], page, job_at)
        })
        .await
    }

    async fn claim_job(
        &self,
        worker: &str,
        kinds: &[String],
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError> {
        let worker = worker.to_owned();
        let kinds = kinds.to_vec();
        self.send(|reply| Command::Claim {
            worker,
            kinds,
            now,
            lease_until,
            reply,
        })
        .await
    }

    async fn complete_job(&self, id: JobId, worker: &str, result: Value) -> Result<(), StoreError> {
        let worker = worker.to_owned();
        self.send(|reply| Command::Complete {
            id,
            worker,
            result,
            reply,
        })
        .await
    }

    async fn fail_job(
        &self,
        id: JobId,
        worker: &str,
        error: &str,
        result: Option<Value>,
        retry_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let (worker, error) = (worker.to_owned(), error.to_owned());
        self.send(|reply| Command::Fail {
            id,
            worker,
            error,
            result,
            retry_at,
            reply,
        })
        .await
    }
}

#[async_trait]
impl SpendLog for DuckDbStore {
    async fn reserve_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        amount: u64,
        cap: u64,
    ) -> Result<bool, StoreError> {
        let provider = provider.to_owned();
        self.send(|reply| Command::ReserveSpend {
            provider,
            day,
            amount,
            cap,
            reply,
        })
        .await
    }

    async fn settle_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        reserved: u64,
        actual: u64,
    ) -> Result<(), StoreError> {
        let provider = provider.to_owned();
        self.send(|reply| Command::SettleSpend {
            provider,
            day,
            reserved,
            actual,
            reply,
        })
        .await
    }

    async fn spent_on(&self, provider: &str, day: NaiveDate) -> Result<u64, StoreError> {
        let provider = provider.to_owned();
        self.send(|reply| Command::SpentOn {
            provider,
            day,
            reply,
        })
        .await
    }
}

#[async_trait]
impl DefinitionReader for DuckDbStore {
    async fn get_definition(&self, key: &DefinitionKey) -> Result<Option<Definition>, StoreError> {
        let key = key.clone();
        self.read(move |connection| {
            optional(connection.query_row(
                &format!(
                    "SELECT {DEFINITION_COLUMNS} FROM definitions WHERE kind = ? AND name = ?"
                ),
                params![definition_kind_name(key.kind), key.name],
                definition_at,
            ))
        })
        .await
    }

    async fn list_definitions(
        &self,
        kind: DefinitionKind,
        page: PageRequest,
    ) -> Result<Page<Definition>, StoreError> {
        self.read(move |connection| {
            let sql = format!(
                "SELECT {DEFINITION_COLUMNS} FROM definitions WHERE kind = ? ORDER BY name"
            );
            paged(
                connection,
                &sql,
                params![definition_kind_name(kind)],
                page,
                definition_at,
            )
        })
        .await
    }
}

#[async_trait]
impl RecordReader for DuckDbStore {
    async fn register_records(&self, decls: &RecordDecls) -> Result<(), StoreError> {
        self.catalog
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .register(decls)
    }

    async fn get_record(
        &self,
        record_type: &str,
        id: RecordId,
    ) -> Result<Option<Record>, StoreError> {
        self.catalog
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .check_filters(record_type, &[])?;
        let record_type = record_type.to_owned();
        self.read(move |connection| {
            optional(connection.query_row(
                &format!(
                    "SELECT {} FROM records r WHERE r.record_type = ? AND r.id = ?",
                    crate::codec::RECORD_COLUMNS
                ),
                params![record_type, id.to_string()],
                crate::codec::record_at,
            ))
        })
        .await
    }

    async fn query_records(&self, query: RecordQuery) -> Result<Page<Record>, StoreError> {
        self.catalog
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .check_filters(&query.record_type, &query.filters)?;
        let catalog = self.catalog_snapshot();
        self.read(move |connection| {
            let catalog = catalog.read().unwrap_or_else(PoisonError::into_inner);
            read::query_records(connection, &catalog, &query)
        })
        .await
    }

    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: Direction,
        page: PageRequest,
    ) -> Result<Page<Record>, StoreError> {
        let target_type = {
            let catalog = self.catalog.read().unwrap_or_else(PoisonError::into_inner);
            let declared = catalog.relation(relation)?;
            match direction {
                Direction::Outgoing => declared.to.clone(),
                Direction::Incoming => declared.from.clone(),
            }
        };
        let relation = relation.to_owned();
        self.read(move |connection| {
            read::neighbors(connection, &relation, &target_type, id, direction, page)
        })
        .await
    }
}

#[async_trait]
impl Store for DuckDbStore {
    async fn apply(&self, unit: UnitOfWork) -> Result<(), StoreError> {
        self.send(|reply| Command::Apply(Box::new(unit), reply))
            .await
    }
}
