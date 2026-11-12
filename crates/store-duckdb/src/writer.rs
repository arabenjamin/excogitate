//! The one writer: a thread that owns the only connection allowed to write.

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, NaiveDate, Utc};
use duckdb::{Connection, params};
use serde_json::Value;
use store::{
    Chunk, ChunkId, Definition, DefinitionKey, Job, JobId, JobStatus, Note, NoteId, Record,
    RecordCatalog, RecordId, StoreError, Task, UnitOfWork, WorkingEntry, WriteOp,
};
use tokio::sync::{mpsc, oneshot};

use crate::codec::{
    JOB_COLUMNS, backend, data_class_name, definition_kind_name, job_at, job_status_name,
    task_status_name, trust_name, vector_param,
};

/// The most job operations served before ingest gets a turn. Serving them
/// without a limit starved ingest under a busy queue: one note in 30 seconds.
const CLAIM_SLICE: usize = 8;

type Reply<T> = oneshot::Sender<Result<T, StoreError>>;

/// A request to the writer.
#[derive(Debug)]
pub(crate) enum Command {
    Apply(Box<UnitOfWork>, Reply<()>),
    Checkpoint(Reply<()>),
    Export(String, Reply<()>),
    Claim {
        worker: String,
        kinds: Vec<String>,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
        reply: Reply<Option<Job>>,
    },
    Complete {
        id: JobId,
        worker: String,
        result: Value,
        reply: Reply<()>,
    },
    Fail {
        id: JobId,
        worker: String,
        error: String,
        result: Option<Value>,
        retry_at: DateTime<Utc>,
        reply: Reply<()>,
    },
    ReserveSpend {
        provider: String,
        day: NaiveDate,
        amount: u64,
        cap: u64,
        reply: Reply<bool>,
    },
    SettleSpend {
        provider: String,
        day: NaiveDate,
        reserved: u64,
        actual: u64,
        reply: Reply<()>,
    },
    SpentOn {
        provider: String,
        day: NaiveDate,
        reply: Reply<u64>,
    },
    PruneJobs {
        before: DateTime<Utc>,
        limit: u32,
        reply: Reply<u32>,
    },
}

impl Command {
    const fn is_job_operation(&self) -> bool {
        matches!(
            self,
            Self::Claim { .. }
                | Self::Complete { .. }
                | Self::Fail { .. }
                | Self::PruneJobs { .. }
                | Self::ReserveSpend { .. }
                | Self::SettleSpend { .. }
                | Self::SpentOn { .. }
        )
    }
}

/// Waiting commands, in the order the writer serves them.
#[derive(Debug)]
struct Queues<C> {
    jobs: VecDeque<C>,
    ingest: VecDeque<C>,
}

impl<C> Queues<C> {
    const fn new() -> Self {
        Self {
            jobs: VecDeque::new(),
            ingest: VecDeque::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.jobs.is_empty() && self.ingest.is_empty()
    }

    fn push(&mut self, command: C, is_job_operation: bool) {
        if is_job_operation {
            self.jobs.push_back(command);
        } else {
            self.ingest.push_back(command);
        }
    }

    /// The next turn: up to a slice of job operations, then one ingest.
    fn next_turn(&mut self) -> Vec<C> {
        let count = self.jobs.len().min(CLAIM_SLICE);
        let mut turn: Vec<C> = self.jobs.drain(..count).collect();
        turn.extend(self.ingest.pop_front());
        turn
    }
}

/// Serves commands until every sender is gone, then checkpoints.
pub(crate) fn run(
    connection: &Connection,
    mut commands: mpsc::Receiver<Command>,
    catalog: &Arc<RwLock<RecordCatalog>>,
    dimension: usize,
) {
    let writer = Writer {
        connection,
        catalog,
        dimension,
    };
    let mut queues = Queues::new();
    loop {
        if queues.is_empty() {
            let Some(command) = commands.blocking_recv() else {
                break;
            };
            let is_job = command.is_job_operation();
            queues.push(command, is_job);
        }
        while let Ok(command) = commands.try_recv() {
            let is_job = command.is_job_operation();
            queues.push(command, is_job);
        }
        for command in queues.next_turn() {
            writer.serve(command);
        }
    }
    // A clean close leaves no write-ahead log to replay. If it fails, the log
    // is still there and DuckDB replays it on the next open.
    let _ = writer.checkpoint();
}

struct Writer<'a> {
    connection: &'a Connection,
    catalog: &'a Arc<RwLock<RecordCatalog>>,
    dimension: usize,
}

#[allow(clippy::needless_pass_by_value, reason = "ids are small and Copy")]
fn id_of(id: impl ToString) -> String {
    id.to_string()
}

fn count(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Invalid("number too large to store".to_owned()))
}

fn to_json(value: &impl serde::Serialize) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(backend)
}

impl Writer<'_> {
    fn serve(&self, command: Command) {
        // A caller that gave up doesn't undo the write; nothing more to do.
        match command {
            Command::Apply(unit, reply) => {
                let _ = reply.send(self.apply(*unit));
            }
            Command::Checkpoint(reply) => {
                let _ = reply.send(self.checkpoint());
            }
            Command::Export(directory, reply) => {
                let _ = reply.send(self.export(&directory));
            }
            Command::Claim {
                worker,
                kinds,
                now,
                lease_until,
                reply,
            } => {
                let _ = reply.send(self.claim(&worker, &kinds, now, lease_until));
            }
            Command::Complete {
                id,
                worker,
                result,
                reply,
            } => {
                let _ = reply.send(self.complete(id, &worker, &result));
            }
            Command::Fail {
                id,
                worker,
                error,
                result,
                retry_at,
                reply,
            } => {
                let _ = reply.send(self.fail(id, &worker, &error, result.as_ref(), retry_at));
            }
            Command::PruneJobs {
                before,
                limit,
                reply,
            } => {
                let _ = reply.send(self.prune_jobs(before, limit));
            }
            Command::ReserveSpend {
                provider,
                day,
                amount,
                cap,
                reply,
            } => {
                let _ = reply.send(self.reserve_spend(&provider, day, amount, cap));
            }
            Command::SettleSpend {
                provider,
                day,
                reserved,
                actual,
                reply,
            } => {
                let _ = reply.send(self.settle_spend(&provider, day, reserved, actual));
            }
            Command::SpentOn {
                provider,
                day,
                reply,
            } => {
                let _ = reply.send(self.spent_on(&provider, day));
            }
        }
    }

    fn transaction<T>(
        &self,
        work: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.connection.execute_batch("BEGIN").map_err(backend)?;
        match work() {
            Ok(value) => {
                self.connection.execute_batch("COMMIT").map_err(backend)?;
                Ok(value)
            }
            Err(error) => {
                // The original error is the one worth reporting.
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn apply(&self, unit: UnitOfWork) -> Result<(), StoreError> {
        self.transaction(|| unit.ops.into_iter().try_for_each(|op| self.apply_op(op)))
    }

    fn checkpoint(&self) -> Result<(), StoreError> {
        // A plain checkpoint is refused while a reader has a transaction open.
        self.connection
            .execute_batch("CHECKPOINT")
            .or_else(|_| self.connection.execute_batch("FORCE CHECKPOINT"))
            .map_err(backend)
    }

    fn export(&self, directory: &str) -> Result<(), StoreError> {
        let quoted = directory.replace('\'', "''");
        self.connection
            .execute_batch(&format!("EXPORT DATABASE '{quoted}' (FORMAT parquet)"))
            .map_err(backend)
    }

    fn exists(&self, sql: &str, parameter: &str) -> Result<bool, StoreError> {
        self.connection
            .query_row(sql, params![parameter], |_| Ok(()))
            .map(|()| true)
            .or_else(|error| match error {
                duckdb::Error::QueryReturnedNoRows => Ok(false),
                other => Err(backend(other)),
            })
    }

    fn note_version(&self, id: NoteId) -> Result<Option<u64>, StoreError> {
        let found = self.connection.query_row(
            "SELECT version FROM notes WHERE id = ?",
            params![id_of(id)],
            |row| row.get::<_, i64>(0),
        );
        match found {
            Ok(version) => u64::try_from(version).map(Some).map_err(backend),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(backend(error)),
        }
    }

    fn insert_note(&self, note: &Note, version: i64) -> Result<(), StoreError> {
        self.connection
            .execute(
                "INSERT INTO notes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    id_of(note.id),
                    note.title,
                    note.body,
                    to_json(&note.tags)?,
                    note.source_url,
                    note.content_hash,
                    data_class_name(note.provenance.data_class),
                    trust_name(note.provenance.trust),
                    note.provenance.stored_by.as_str(),
                    note.provenance.stored_at,
                    version,
                    note.updated_at,
                ],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn remove_chunks(&self, note: NoteId) -> Result<(), StoreError> {
        let id = id_of(note);
        self.connection
            .execute(
                "DELETE FROM chunk_vectors WHERE chunk_id IN
                 (SELECT id FROM chunks WHERE note_id = ?)",
                params![id],
            )
            .map_err(backend)?;
        self.connection
            .execute("DELETE FROM chunks WHERE note_id = ?", params![id])
            .map_err(backend)?;
        Ok(())
    }

    fn replace_chunks(&self, note_id: NoteId, chunks: Vec<Chunk>) -> Result<(), StoreError> {
        if self.note_version(note_id)?.is_none() {
            return Err(StoreError::NotFound(format!("note {note_id}")));
        }
        let mut seen = BTreeSet::<ChunkId>::new();
        for chunk in &chunks {
            if chunk.note_id != note_id {
                return Err(StoreError::Invalid(format!(
                    "chunk {} belongs to another note",
                    chunk.id
                )));
            }
            if chunk
                .embedding
                .as_ref()
                .is_some_and(|v| v.len() != self.dimension)
            {
                return Err(StoreError::Invalid(format!(
                    "chunk {}: embedding must have width {}",
                    chunk.id, self.dimension
                )));
            }
            if !seen.insert(chunk.id) {
                return Err(StoreError::Conflict(format!("chunk {} repeats", chunk.id)));
            }
        }
        // Delete and insert: the incremental full-text index ignores UPDATE.
        self.remove_chunks(note_id)?;
        let vector_sql = format!(
            "INSERT INTO chunk_vectors VALUES (?, ?::FLOAT[{}])",
            self.dimension
        );
        for chunk in chunks {
            self.connection
                .execute(
                    "INSERT INTO chunks VALUES (?, ?, ?, ?, ?)",
                    params![
                        id_of(chunk.id),
                        id_of(chunk.note_id),
                        chunk.ordinal,
                        chunk.text,
                        chunk.token_count,
                    ],
                )
                .map_err(backend)?;
            if let Some(vector) = &chunk.embedding {
                self.connection
                    .execute(&vector_sql, params![id_of(chunk.id), vector_param(vector)])
                    .map_err(backend)?;
            }
        }
        Ok(())
    }

    fn insert_job(&self, job: &Job) -> Result<(), StoreError> {
        self.connection
            .execute(
                &format!(
                    "INSERT INTO jobs ({JOB_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)"
                ),
                params![
                    id_of(job.id),
                    job.kind,
                    to_json(&job.payload)?,
                    job.priority,
                    job_status_name(job.status),
                    job.requested_by.as_str(),
                    job.run_at,
                    job.attempts,
                    job.max_attempts,
                    job.lease.as_ref().map(|lease| lease.worker.clone()),
                    job.lease.as_ref().map(|lease| lease.until),
                    job.last_error,
                    job.result.as_ref().map(to_json).transpose()?,
                ],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn catalog(&self) -> std::sync::RwLockReadGuard<'_, RecordCatalog> {
        self.catalog
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn put_record(&self, record: &Record) -> Result<(), StoreError> {
        self.catalog().check_record(record)?;
        let (record_type, id) = (&record.record_type, id_of(record.id));
        self.connection
            .execute(
                "DELETE FROM records WHERE record_type = ? AND id = ?",
                params![record_type, id],
            )
            .map_err(backend)?;
        self.connection
            .execute(
                "INSERT INTO records VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    record_type,
                    id,
                    to_json(&record.data)?,
                    data_class_name(record.provenance.data_class),
                    trust_name(record.provenance.trust),
                    record.provenance.stored_by.as_str(),
                    record.provenance.stored_at,
                ],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn record_exists(&self, record_type: &str, id: RecordId) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT count(*) FROM records WHERE record_type = ? AND id = ?",
                params![record_type, id_of(id)],
                |row| row.get::<_, i64>(0),
            )
            .map(|found| found > 0)
            .map_err(backend)
    }

    fn delete_record(&self, record_type: &str, id: RecordId) -> Result<(), StoreError> {
        if !self.record_exists(record_type, id)? {
            return Err(StoreError::NotFound(format!("{record_type} {id}")));
        }
        self.connection
            .execute(
                "DELETE FROM records WHERE record_type = ? AND id = ?",
                params![record_type, id_of(id)],
            )
            .map_err(backend)?;
        let relations: Vec<(String, bool, bool)> = {
            let catalog = self.catalog();
            let names = self.link_relations()?;
            names
                .into_iter()
                .filter_map(|name| {
                    let declared = catalog.relation(&name).ok()?;
                    let (from, to) = (declared.from == record_type, declared.to == record_type);
                    (from || to).then_some((name, from, to))
                })
                .collect()
        };
        for (relation, from, to) in relations {
            self.connection
                .execute(
                    "DELETE FROM links WHERE relation = ? AND
                     ((? AND from_id = ?) OR (? AND to_id = ?))",
                    params![relation, from, id_of(id), to, id_of(id)],
                )
                .map_err(backend)?;
        }
        Ok(())
    }

    fn link_relations(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT relation FROM links")
            .map_err(backend)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(backend)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(backend)
    }

    fn link(&self, relation: &str, from: RecordId, to: RecordId) -> Result<(), StoreError> {
        let (from_type, to_type) = {
            let catalog = self.catalog();
            let declared = catalog.relation(relation)?;
            (declared.from.clone(), declared.to.clone())
        };
        for (record_type, id) in [(&from_type, from), (&to_type, to)] {
            if !self.record_exists(record_type, id)? {
                return Err(StoreError::NotFound(format!("{record_type} {id}")));
            }
        }
        self.unlink(relation, from, to)?;
        self.connection
            .execute(
                "INSERT INTO links VALUES (?, ?, ?)",
                params![relation, id_of(from), id_of(to)],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn unlink(&self, relation: &str, from: RecordId, to: RecordId) -> Result<(), StoreError> {
        self.connection
            .execute(
                "DELETE FROM links WHERE relation = ? AND from_id = ? AND to_id = ?",
                params![relation, id_of(from), id_of(to)],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn update_note(&self, note: &Note, expected_version: u64) -> Result<(), StoreError> {
        let stored = self
            .note_version(note.id)?
            .ok_or_else(|| StoreError::NotFound(format!("note {}", note.id)))?;
        if stored != expected_version {
            return Err(StoreError::Conflict(format!(
                "note {} is at version {stored}, not {expected_version}",
                note.id
            )));
        }
        self.delete_row("DELETE FROM notes WHERE id = ?", &id_of(note.id))?;
        self.insert_note(note, count(expected_version + 1)?)
    }

    fn delete_note(&self, id: NoteId) -> Result<(), StoreError> {
        if self.note_version(id)?.is_none() {
            return Err(StoreError::NotFound(format!("note {id}")));
        }
        self.remove_chunks(id)?;
        self.delete_row("DELETE FROM notes WHERE id = ?", &id_of(id))?;
        Ok(())
    }

    /// Runs a one-parameter delete, returning how many rows went.
    fn delete_row(&self, sql: &str, id: &str) -> Result<usize, StoreError> {
        self.connection.execute(sql, params![id]).map_err(backend)
    }

    fn put_task(&self, task: &Task) -> Result<(), StoreError> {
        self.delete_row("DELETE FROM tasks WHERE id = ?", &id_of(task.id))?;
        self.connection
            .execute(
                "INSERT INTO tasks VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    id_of(task.id),
                    task.title,
                    task.detail,
                    task_status_name(task.status),
                    task.due_at,
                    task.created_at,
                ],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn delete_working(&self, scope: &str, key: &str) -> Result<(), StoreError> {
        self.connection
            .execute(
                "DELETE FROM working WHERE scope = ? AND key = ?",
                params![scope, key],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn put_working(&self, entry: &WorkingEntry) -> Result<(), StoreError> {
        self.delete_working(&entry.scope, &entry.key)?;
        self.connection
            .execute(
                "INSERT INTO working VALUES (?, ?, ?, ?)",
                params![
                    entry.scope,
                    entry.key,
                    to_json(&entry.value)?,
                    entry.expires_at
                ],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn delete_definition(&self, key: &DefinitionKey) -> Result<(), StoreError> {
        self.connection
            .execute(
                "DELETE FROM definitions WHERE kind = ? AND name = ?",
                params![definition_kind_name(key.kind), key.name],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn put_definition(&self, definition: &Definition) -> Result<(), StoreError> {
        self.delete_definition(&definition.key)?;
        self.connection
            .execute(
                "INSERT INTO definitions VALUES (?, ?, ?, ?, ?)",
                params![
                    definition_kind_name(definition.key.kind),
                    definition.key.name,
                    to_json(&definition.body)?,
                    definition.source,
                    definition.updated_at,
                ],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn apply_op(&self, op: WriteOp) -> Result<(), StoreError> {
        match op {
            WriteOp::CreateNote(note) => {
                if self.note_version(note.id)?.is_some() {
                    return Err(StoreError::Conflict(format!("note {} exists", note.id)));
                }
                self.insert_note(&note, 1)
            }
            WriteOp::UpdateNote {
                note,
                expected_version,
            } => self.update_note(&note, expected_version),
            WriteOp::DeleteNote(id) => self.delete_note(id),
            WriteOp::ReplaceChunks { note_id, chunks } => self.replace_chunks(note_id, chunks),
            WriteOp::PutTask(task) => self.put_task(&task),
            WriteOp::DeleteTask(id) => {
                if self.delete_row("DELETE FROM tasks WHERE id = ?", &id_of(id))? == 0 {
                    return Err(StoreError::NotFound(format!("task {id}")));
                }
                Ok(())
            }
            WriteOp::PutWorking(entry) => self.put_working(&entry),
            WriteOp::DeleteWorking { scope, key } => self.delete_working(&scope, &key),
            WriteOp::EnqueueJob(job) => {
                if self.exists("SELECT 1 FROM jobs WHERE id = ?", &id_of(job.id))? {
                    return Err(StoreError::Conflict(format!("job {} exists", job.id)));
                }
                self.insert_job(&job)
            }
            WriteOp::PutDefinition(definition) => self.put_definition(&definition),
            WriteOp::DeleteDefinition(key) => self.delete_definition(&key),
            WriteOp::PutRecord(record) => self.put_record(&record),
            WriteOp::DeleteRecord { record_type, id } => self.delete_record(&record_type, id),
            WriteOp::Link { relation, from, to } => self.link(&relation, from, to),
            WriteOp::Unlink { relation, from, to } => {
                self.catalog().relation(&relation)?;
                self.unlink(&relation, from, to)
            }
        }
    }

    fn claim(
        &self,
        worker: &str,
        kinds: &[String],
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError> {
        if lease_until <= now {
            return Err(StoreError::Invalid("a lease must end after now".to_owned()));
        }
        // Only the count of kinds goes into the text; the kinds are bound.
        let places = vec!["?"; kinds.len()].join(", ");
        self.transaction(|| {
            self.connection
                .execute(
                    "UPDATE jobs SET status = 'dead', lease_worker = NULL, lease_until = NULL,
                         last_error = 'lease lapsed on the last attempt', finished_at = ?
                     WHERE status = 'claimed' AND lease_until <= ? AND attempts >= max_attempts",
                    params![now, now],
                )
                .map_err(backend)?;
            if kinds.is_empty() {
                return Ok(None);
            }
            let sql = format!(
                "SELECT id FROM jobs
                 WHERE ((status = 'queued' AND run_at <= ?)
                    OR (status = 'claimed' AND lease_until <= ?))
                   AND kind IN ({places})
                 ORDER BY priority DESC, run_at, id LIMIT 1"
            );
            let mut bound: Vec<&dyn duckdb::ToSql> = vec![&now, &now];
            bound.extend(kinds.iter().map(|kind| kind as &dyn duckdb::ToSql));
            let next = self
                .connection
                .query_row(&sql, duckdb::params_from_iter(bound), |row| {
                    row.get::<_, String>(0)
                });
            let id = match next {
                Ok(id) => id,
                Err(duckdb::Error::QueryReturnedNoRows) => return Ok(None),
                Err(error) => return Err(backend(error)),
            };
            self.connection
                .execute(
                    "UPDATE jobs SET status = 'claimed', attempts = attempts + 1,
                         lease_worker = ?, lease_until = ? WHERE id = ?",
                    params![worker, lease_until, id],
                )
                .map_err(backend)?;
            self.connection
                .query_row(
                    &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?"),
                    params![id],
                    job_at,
                )
                .map(Some)
                .map_err(backend)
        })
    }

    fn held_job(&self, id: JobId, worker: &str) -> Result<Job, StoreError> {
        let found = self.connection.query_row(
            &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?"),
            params![id_of(id)],
            job_at,
        );
        let job = match found {
            Ok(job) => job,
            Err(duckdb::Error::QueryReturnedNoRows) => {
                return Err(StoreError::NotFound(format!("job {id}")));
            }
            Err(error) => return Err(backend(error)),
        };
        let holds = job.status == JobStatus::Claimed
            && job
                .lease
                .as_ref()
                .is_some_and(|lease| lease.worker == worker);
        if holds {
            Ok(job)
        } else {
            Err(StoreError::Conflict(format!(
                "job {id} isn't held by {worker}"
            )))
        }
    }

    fn complete(&self, id: JobId, worker: &str, result: &Value) -> Result<(), StoreError> {
        self.held_job(id, worker)?;
        self.connection
            .execute(
                "UPDATE jobs SET status = 'succeeded', lease_worker = NULL, lease_until = NULL,
                     result = ?, finished_at = ? WHERE id = ?",
                params![to_json(result)?, Utc::now(), id_of(id)],
            )
            .map(|_| ())
            .map_err(backend)
    }

    fn prune_jobs(&self, before: DateTime<Utc>, limit: u32) -> Result<u32, StoreError> {
        let pruned = self
            .connection
            .execute(
                "DELETE FROM jobs WHERE id IN (
                     SELECT id FROM jobs
                     WHERE status IN ('succeeded', 'dead') AND coalesce(finished_at, run_at) < ?
                     ORDER BY coalesce(finished_at, run_at), id LIMIT ?)",
                params![before, limit],
            )
            .map_err(backend)?;
        Ok(u32::try_from(pruned).unwrap_or(u32::MAX))
    }

    fn spent_on(&self, provider: &str, day: NaiveDate) -> Result<u64, StoreError> {
        let found = self.connection.query_row(
            "SELECT usd_micros FROM spend WHERE provider = ? AND day = ?",
            params![provider, day],
            |row| row.get::<_, u64>(0),
        );
        match found {
            Ok(spent) => Ok(spent),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(0),
            Err(error) => Err(backend(error)),
        }
    }

    /// Writes a day's total. No primary key, as with the other tables, so
    /// it's one writer's update-or-insert.
    fn set_spend(&self, provider: &str, day: NaiveDate, total: u64) -> Result<(), StoreError> {
        let changed = self
            .connection
            .execute(
                "UPDATE spend SET usd_micros = ? WHERE provider = ? AND day = ?",
                params![total, provider, day],
            )
            .map_err(backend)?;
        if changed == 0 {
            self.connection
                .execute(
                    "INSERT INTO spend VALUES (?, ?, ?)",
                    params![provider, day, total],
                )
                .map_err(backend)?;
        }
        Ok(())
    }

    fn reserve_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        amount: u64,
        cap: u64,
    ) -> Result<bool, StoreError> {
        self.transaction(|| {
            let spent = self.spent_on(provider, day)?;
            match spent.checked_add(amount) {
                Some(total) if total <= cap => {
                    self.set_spend(provider, day, total)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
    }

    fn settle_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        reserved: u64,
        actual: u64,
    ) -> Result<(), StoreError> {
        self.transaction(|| {
            let spent = self.spent_on(provider, day)?;
            self.set_spend(
                provider,
                day,
                spent.saturating_sub(reserved).saturating_add(actual),
            )
        })
    }

    fn fail(
        &self,
        id: JobId,
        worker: &str,
        error: &str,
        result: Option<&Value>,
        retry_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let job = self.held_job(id, worker)?;
        let left = result.map(to_json).transpose()?;
        let sql = if job.attempts >= job.max_attempts {
            "UPDATE jobs SET status = 'dead', lease_worker = NULL, lease_until = NULL,
                 last_error = ?, result = ?, finished_at = ? WHERE id = ?"
        } else {
            "UPDATE jobs SET status = 'queued', lease_worker = NULL, lease_until = NULL,
                 last_error = ?, result = ?, run_at = ? WHERE id = ?"
        };
        let done = if job.attempts >= job.max_attempts {
            self.connection
                .execute(sql, params![error, left, Utc::now(), id_of(id)])
        } else {
            self.connection
                .execute(sql, params![error, left, retry_at, id_of(id)])
        };
        done.map(|_| ()).map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_gets_a_turn_under_a_busy_queue() {
        let mut queues = Queues::new();
        for n in 0..20 {
            queues.push(n, true);
        }
        queues.push(100, false);
        let first = queues.next_turn();
        assert_eq!(first.len(), CLAIM_SLICE + 1);
        assert_eq!(first.last(), Some(&100));
    }

    #[test]
    fn job_operations_go_first_and_in_order() {
        let mut queues = Queues::new();
        queues.push(100, false);
        queues.push(1, true);
        queues.push(2, true);
        assert_eq!(queues.next_turn(), [1, 2, 100]);
        assert!(queues.is_empty());
    }

    #[test]
    fn one_ingest_runs_per_turn() {
        let mut queues = Queues::new();
        queues.push(1, false);
        queues.push(2, false);
        assert_eq!(queues.next_turn(), [1]);
        assert_eq!(queues.next_turn(), [2]);
    }
}
