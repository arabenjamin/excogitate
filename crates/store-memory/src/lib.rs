//! A store in memory. It's for tests, and it's deliberately plain: a
//! conformance suite that passes here and on a real database isn't written
//! around that database.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, Utc};
use protocol::DataClass;
use serde_json::Value;
use store::{
    Archive, Chunk, ChunkHit, ChunkId, ClassFilter, Definition, DefinitionKey, DefinitionKind,
    DefinitionReader, Direction, Job, JobId, JobQueue, JobStatus, Lease, Link, Note, NoteId,
    NoteReader, NotesStored, Page, PageRequest, Record, RecordCatalog, RecordDecls, RecordId,
    RecordQuery, RecordReader, SpendDay, SpendLog, Store, StoreError, StoredWindow, Task, TaskId,
    TaskQuery, TaskReader, UnitOfWork, WorkingEntry, WriteOp,
};

#[derive(Debug, Clone, Default)]
struct State {
    notes: BTreeMap<NoteId, Note>,
    chunks: BTreeMap<NoteId, Vec<Chunk>>,
    tasks: BTreeMap<TaskId, Task>,
    working: BTreeMap<(String, String), WorkingEntry>,
    jobs: BTreeMap<JobId, Job>,
    definitions: BTreeMap<DefinitionKey, Definition>,
    records: BTreeMap<(String, RecordId), Record>,
    links: BTreeSet<(String, RecordId, RecordId)>,
    catalog: RecordCatalog,
    spend: BTreeMap<(String, NaiveDate), u64>,
}

/// A complete store held in a mutex.
#[derive(Debug)]
pub struct MemoryStore {
    dimension: usize,
    state: Mutex<State>,
}

impl MemoryStore {
    /// An empty store for embeddings of this width.
    #[must_use]
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension,
            state: Mutex::new(State::default()),
        }
    }

    /// State is replaced whole after a successful write, so a poisoned lock
    /// still guards a consistent value.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    let denominator = norm(a) * norm(b);
    if denominator == 0.0 {
        0.0
    } else {
        dot / denominator
    }
}

fn admits(classes: ClassFilter, note: &Note) -> bool {
    classes == ClassFilter::Any || note.provenance.data_class == DataClass::Public
}

impl State {
    /// Every chunk the filter admits, scored; a `None` score leaves it out.
    fn search(
        &self,
        classes: ClassFilter,
        page: PageRequest,
        score: impl Fn(&Chunk) -> Option<f32>,
    ) -> Page<ChunkHit> {
        let mut hits: Vec<ChunkHit> = self
            .notes
            .values()
            .filter(|note| admits(classes, note))
            .flat_map(|note| {
                let chunks = self.chunks.get(&note.id).map_or(&[][..], Vec::as_slice);
                chunks.iter().filter_map(|chunk| {
                    score(chunk).map(|score| ChunkHit {
                        chunk: Chunk {
                            embedding: None,
                            ..chunk.clone()
                        },
                        provenance: note.provenance.clone(),
                        score,
                    })
                })
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.chunk.id.cmp(&b.chunk.id))
        });
        Page::from_sorted(hits, page)
    }

    fn apply_op(&mut self, op: WriteOp, dimension: usize) -> Result<(), StoreError> {
        match op {
            WriteOp::CreateNote(mut note) => {
                if self.notes.contains_key(&note.id) {
                    return Err(StoreError::Conflict(format!("note {} exists", note.id)));
                }
                note.version = 1;
                self.notes.insert(note.id, note);
            }
            WriteOp::UpdateNote {
                mut note,
                expected_version,
            } => {
                let stored = self
                    .notes
                    .get(&note.id)
                    .ok_or_else(|| StoreError::NotFound(format!("note {}", note.id)))?;
                if stored.version != expected_version {
                    return Err(StoreError::Conflict(format!(
                        "note {} is at version {}, not {expected_version}",
                        note.id, stored.version
                    )));
                }
                note.version = expected_version + 1;
                self.notes.insert(note.id, note);
            }
            WriteOp::DeleteNote(id) => {
                self.notes
                    .remove(&id)
                    .ok_or_else(|| StoreError::NotFound(format!("note {id}")))?;
                self.chunks.remove(&id);
            }
            WriteOp::ReplaceChunks { note_id, chunks } => {
                self.replace_chunks(note_id, chunks, dimension)?;
            }
            WriteOp::PutTask(task) => {
                self.tasks.insert(task.id, task);
            }
            WriteOp::DeleteTask(id) => {
                self.tasks
                    .remove(&id)
                    .ok_or_else(|| StoreError::NotFound(format!("task {id}")))?;
            }
            WriteOp::PutWorking(entry) => {
                self.working
                    .insert((entry.scope.clone(), entry.key.clone()), entry);
            }
            WriteOp::DeleteWorking { scope, key } => {
                self.working.remove(&(scope, key));
            }
            WriteOp::EnqueueJob(mut job) => {
                if self.jobs.contains_key(&job.id) {
                    return Err(StoreError::Conflict(format!("job {} exists", job.id)));
                }
                job.finished_at = None;
                self.jobs.insert(job.id, job);
            }
            WriteOp::RestoreJob(job) => {
                if self.jobs.contains_key(&job.id) {
                    return Err(StoreError::Conflict(format!("job {} exists", job.id)));
                }
                self.jobs.insert(job.id, job);
            }
            WriteOp::PutDefinition(definition) => {
                self.definitions.insert(definition.key.clone(), definition);
            }
            WriteOp::DeleteDefinition(key) => {
                self.definitions.remove(&key);
            }
            WriteOp::PutRecord(record) => {
                self.catalog.check_record(&record)?;
                self.records
                    .insert((record.record_type.clone(), record.id), record);
            }
            WriteOp::DeleteRecord { record_type, id } => {
                self.records
                    .remove(&(record_type.clone(), id))
                    .ok_or_else(|| StoreError::NotFound(format!("{record_type} {id}")))?;
                self.remove_links_of(&record_type, id);
            }
            WriteOp::Link { relation, from, to } => {
                let decl = self.catalog.relation(&relation)?;
                for (record_type, id) in [(&decl.from, from), (&decl.to, to)] {
                    if !self.records.contains_key(&(record_type.clone(), id)) {
                        return Err(StoreError::NotFound(format!("{record_type} {id}")));
                    }
                }
                self.links.insert((relation, from, to));
            }
            WriteOp::Unlink { relation, from, to } => {
                self.catalog.relation(&relation)?;
                self.links.remove(&(relation, from, to));
            }
        }
        Ok(())
    }

    fn replace_chunks(
        &mut self,
        note_id: NoteId,
        mut chunks: Vec<Chunk>,
        dimension: usize,
    ) -> Result<(), StoreError> {
        if !self.notes.contains_key(&note_id) {
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
                .is_some_and(|v| v.len() != dimension)
            {
                return Err(StoreError::Invalid(format!(
                    "chunk {}: embedding must have width {dimension}",
                    chunk.id
                )));
            }
            if !seen.insert(chunk.id) {
                return Err(StoreError::Conflict(format!("chunk {} repeats", chunk.id)));
            }
        }
        chunks.sort_by_key(|chunk| chunk.ordinal);
        self.chunks.insert(note_id, chunks);
        Ok(())
    }

    fn remove_links_of(&mut self, record_type: &str, id: RecordId) {
        let catalog = &self.catalog;
        self.links.retain(|(relation, from, to)| {
            let Ok(decl) = catalog.relation(relation) else {
                return true;
            };
            !((decl.from == record_type && *from == id) || (decl.to == record_type && *to == id))
        });
    }

    /// Marks jobs dead whose lease lapsed on their last attempt.
    fn bury_lapsed(&mut self, now: DateTime<Utc>) {
        for job in self.jobs.values_mut() {
            if lease_lapsed(job, now) && job.attempts >= job.max_attempts {
                job.status = JobStatus::Dead;
                job.lease = None;
                job.last_error = Some("lease lapsed on the last attempt".to_owned());
                job.finished_at = Some(now);
            }
        }
    }

    fn held_job(&mut self, id: JobId, worker: &str) -> Result<&mut Job, StoreError> {
        let job = self
            .jobs
            .get_mut(&id)
            .ok_or_else(|| StoreError::NotFound(format!("job {id}")))?;
        if job.status != JobStatus::Claimed
            || job
                .lease
                .as_ref()
                .is_none_or(|lease| lease.worker != worker)
        {
            return Err(StoreError::Conflict(format!(
                "job {id} isn't held by {worker}"
            )));
        }
        Ok(job)
    }
}

fn lease_lapsed(job: &Job, now: DateTime<Utc>) -> bool {
    job.status == JobStatus::Claimed && job.lease.as_ref().is_some_and(|lease| lease.until <= now)
}

fn runnable(job: &Job, now: DateTime<Utc>) -> bool {
    (job.status == JobStatus::Queued && job.run_at <= now) || lease_lapsed(job, now)
}

#[async_trait]
impl NoteReader for MemoryStore {
    fn embedding_dimension(&self) -> usize {
        self.dimension
    }

    async fn get_note(&self, id: NoteId) -> Result<Option<Note>, StoreError> {
        Ok(self.state().notes.get(&id).cloned())
    }

    async fn find_note_by_hash(
        &self,
        hash: &str,
        class: DataClass,
    ) -> Result<Option<Note>, StoreError> {
        // Notes are kept in id order, so the first match is the lowest id.
        Ok(self
            .state()
            .notes
            .values()
            .find(|note| note.content_hash == hash && note.provenance.data_class == class)
            .cloned())
    }

    async fn list_chunks(&self, note_id: NoteId) -> Result<Vec<Chunk>, StoreError> {
        Ok(self
            .state()
            .chunks
            .get(&note_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn notes_stored(
        &self,
        window: StoredWindow,
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<NotesStored, StoreError> {
        let mut held: Vec<Note> = self
            .state()
            .notes
            .values()
            .filter(|note| admits(classes, note) && window.holds(note.provenance.stored_at))
            .cloned()
            .collect();
        held.sort_by(|a, b| {
            b.provenance
                .stored_at
                .cmp(&a.provenance.stored_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let total = held.len() as u64;
        Ok(NotesStored {
            notes: Page::from_sorted(held, page),
            total,
        })
    }

    async fn search_text(
        &self,
        query: &str,
        classes: ClassFilter,
        page: PageRequest,
    ) -> Result<Page<ChunkHit>, StoreError> {
        let terms: BTreeSet<String> = tokens(query).into_iter().collect();
        Ok(self.state().search(classes, page, |chunk| {
            let matched = tokens(&chunk.text)
                .iter()
                .filter(|token| terms.contains(*token))
                .count();
            // A chunk's term count is far below f32's exact integer range.
            #[expect(clippy::cast_precision_loss, reason = "small counts")]
            (matched > 0).then_some(matched as f32)
        }))
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
        Ok(self.state().search(classes, page, |chunk| {
            chunk.embedding.as_ref().map(|v| cosine(v, vector))
        }))
    }
}

#[async_trait]
impl Archive for MemoryStore {
    async fn list_notes(&self, page: PageRequest) -> Result<Page<Note>, StoreError> {
        Ok(Page::from_sorted(
            self.state().notes.values().cloned().collect::<Vec<_>>(),
            page,
        ))
    }

    async fn list_working(
        &self,
        now: DateTime<Utc>,
        page: PageRequest,
    ) -> Result<Page<WorkingEntry>, StoreError> {
        let state = self.state();
        let live: Vec<WorkingEntry> = state
            .working
            .values()
            .filter(|entry| entry.expires_at.is_none_or(|at| at > now))
            .cloned()
            .collect();
        Ok(Page::from_sorted(live, page))
    }

    async fn list_all_records(&self, page: PageRequest) -> Result<Page<Record>, StoreError> {
        Ok(Page::from_sorted(
            self.state().records.values().cloned().collect::<Vec<_>>(),
            page,
        ))
    }

    async fn list_links(&self, page: PageRequest) -> Result<Page<Link>, StoreError> {
        let state = self.state();
        let links: Vec<Link> = state
            .links
            .iter()
            .map(|(relation, from, to)| Link {
                relation: relation.clone(),
                from: *from,
                to: *to,
            })
            .collect();
        Ok(Page::from_sorted(links, page))
    }

    async fn list_spend(&self, page: PageRequest) -> Result<Page<SpendDay>, StoreError> {
        let state = self.state();
        let days: Vec<SpendDay> = state
            .spend
            .iter()
            .map(|((provider, day), usd_micros)| SpendDay {
                provider: provider.clone(),
                day: *day,
                usd_micros: *usd_micros,
            })
            .collect();
        Ok(Page::from_sorted(days, page))
    }
}

#[async_trait]
impl TaskReader for MemoryStore {
    async fn get_task(&self, id: TaskId) -> Result<Option<Task>, StoreError> {
        Ok(self.state().tasks.get(&id).cloned())
    }

    async fn list_tasks(
        &self,
        query: TaskQuery,
        page: PageRequest,
    ) -> Result<Page<Task>, StoreError> {
        let state = self.state();
        let mut tasks: Vec<&Task> = state
            .tasks
            .values()
            .filter(|task| query.status.is_none_or(|status| task.status == status))
            .filter(|task| {
                query
                    .due_by
                    .is_none_or(|by| task.due_at.is_some_and(|due| due <= by))
            })
            .collect();
        tasks.sort_by_key(|task| (task.created_at, task.id));
        Ok(Page::from_sorted(tasks.into_iter().cloned(), page))
    }

    async fn get_working(
        &self,
        scope: &str,
        key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<WorkingEntry>, StoreError> {
        Ok(self
            .state()
            .working
            .get(&(scope.to_owned(), key.to_owned()))
            .filter(|entry| entry.expires_at.is_none_or(|at| at > now))
            .cloned())
    }
}

#[async_trait]
impl SpendLog for MemoryStore {
    async fn reserve_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        amount: u64,
        cap: u64,
    ) -> Result<bool, StoreError> {
        let mut state = self.state();
        let spent = state.spend.entry((provider.to_owned(), day)).or_default();
        Ok(match spent.checked_add(amount) {
            Some(total) if total <= cap => {
                *spent = total;
                true
            }
            _ => false,
        })
    }

    async fn settle_spend(
        &self,
        provider: &str,
        day: NaiveDate,
        reserved: u64,
        actual: u64,
    ) -> Result<(), StoreError> {
        let mut state = self.state();
        let spent = state.spend.entry((provider.to_owned(), day)).or_default();
        *spent = spent.saturating_sub(reserved).saturating_add(actual);
        Ok(())
    }

    async fn spent_on(&self, provider: &str, day: NaiveDate) -> Result<u64, StoreError> {
        Ok(self
            .state()
            .spend
            .get(&(provider.to_owned(), day))
            .copied()
            .unwrap_or(0))
    }
}

#[async_trait]
impl JobQueue for MemoryStore {
    async fn get_job(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        Ok(self.state().jobs.get(&id).cloned())
    }

    async fn list_jobs(
        &self,
        status: Option<JobStatus>,
        page: PageRequest,
    ) -> Result<Page<Job>, StoreError> {
        let state = self.state();
        let mut jobs: Vec<&Job> = state
            .jobs
            .values()
            .filter(|job| status.is_none_or(|wanted| job.status == wanted))
            .collect();
        jobs.sort_by_key(|job| (std::cmp::Reverse(job.run_at), job.id));
        Ok(Page::from_sorted(jobs.into_iter().cloned(), page))
    }

    async fn claim_job(
        &self,
        worker: &str,
        kinds: &[String],
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError> {
        if lease_until <= now {
            return Err(StoreError::Invalid("a lease must end after now".to_owned()));
        }
        let mut state = self.state();
        state.bury_lapsed(now);
        let Some(job) = state
            .jobs
            .values_mut()
            .filter(|job| runnable(job, now) && kinds.contains(&job.kind))
            .min_by_key(|job| (std::cmp::Reverse(job.priority), job.run_at, job.id))
        else {
            return Ok(None);
        };
        job.status = JobStatus::Claimed;
        job.attempts += 1;
        job.lease = Some(Lease {
            worker: worker.to_owned(),
            until: lease_until,
        });
        Ok(Some(job.clone()))
    }

    async fn complete_job(&self, id: JobId, worker: &str, result: Value) -> Result<(), StoreError> {
        let mut state = self.state();
        let job = state.held_job(id, worker)?;
        job.status = JobStatus::Succeeded;
        job.result = Some(result);
        job.lease = None;
        job.finished_at = Some(Utc::now());
        Ok(())
    }

    async fn fail_job(
        &self,
        id: JobId,
        worker: &str,
        error: &str,
        result: Option<Value>,
        retry_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut state = self.state();
        let job = state.held_job(id, worker)?;
        job.lease = None;
        job.last_error = Some(error.to_owned());
        job.result = result;
        if job.attempts >= job.max_attempts {
            job.status = JobStatus::Dead;
            job.finished_at = Some(Utc::now());
        } else {
            job.status = JobStatus::Queued;
            job.run_at = retry_at;
        }
        Ok(())
    }

    async fn prune_jobs(&self, before: DateTime<Utc>, limit: u32) -> Result<u32, StoreError> {
        let mut state = self.state();
        let mut finished: Vec<(DateTime<Utc>, JobId)> = state
            .jobs
            .values()
            .filter(|job| matches!(job.status, JobStatus::Succeeded | JobStatus::Dead))
            .map(|job| (job.finished_at.unwrap_or(job.run_at), job.id))
            .filter(|(at, _)| *at < before)
            .collect();
        finished.sort();
        finished.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        for (_, id) in &finished {
            state.jobs.remove(id);
        }
        Ok(u32::try_from(finished.len()).unwrap_or(u32::MAX))
    }
}

#[async_trait]
impl DefinitionReader for MemoryStore {
    async fn get_definition(&self, key: &DefinitionKey) -> Result<Option<Definition>, StoreError> {
        Ok(self.state().definitions.get(key).cloned())
    }

    async fn list_definitions(
        &self,
        kind: DefinitionKind,
        page: PageRequest,
    ) -> Result<Page<Definition>, StoreError> {
        let state = self.state();
        let matching = state
            .definitions
            .values()
            .filter(|definition| definition.key.kind == kind)
            .cloned();
        Ok(Page::from_sorted(matching, page))
    }
}

#[async_trait]
impl RecordReader for MemoryStore {
    async fn register_records(&self, decls: &RecordDecls) -> Result<(), StoreError> {
        self.state().catalog.register(decls)
    }

    async fn get_record(
        &self,
        record_type: &str,
        id: RecordId,
    ) -> Result<Option<Record>, StoreError> {
        let state = self.state();
        state.catalog.check_filters(record_type, &[])?;
        Ok(state.records.get(&(record_type.to_owned(), id)).cloned())
    }

    async fn query_records(&self, query: RecordQuery) -> Result<Page<Record>, StoreError> {
        let state = self.state();
        state
            .catalog
            .check_filters(&query.record_type, &query.filters)?;
        let matching = state
            .records
            .values()
            .filter(|record| record.record_type == query.record_type)
            .filter(|record| state.catalog.matches(record, &query.filters))
            .cloned();
        Ok(Page::from_sorted(matching, query.page))
    }

    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: Direction,
        page: PageRequest,
    ) -> Result<Page<Record>, StoreError> {
        let state = self.state();
        let decl = state.catalog.relation(relation)?;
        let target_type = match direction {
            Direction::Outgoing => &decl.to,
            Direction::Incoming => &decl.from,
        };
        let mut targets: Vec<RecordId> = state
            .links
            .iter()
            .filter(|(name, ..)| name == relation)
            .filter_map(|(_, from, to)| match direction {
                Direction::Outgoing => (*from == id).then_some(*to),
                Direction::Incoming => (*to == id).then_some(*from),
            })
            .collect();
        targets.sort_unstable();
        let records = targets
            .into_iter()
            .filter_map(|target| state.records.get(&(target_type.clone(), target)).cloned());
        Ok(Page::from_sorted(records, page))
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn apply(&self, unit: UnitOfWork) -> Result<(), StoreError> {
        let mut guard = self.state();
        let mut next = guard.clone();
        for op in unit.ops {
            next.apply_op(op, self.dimension)?;
        }
        *guard = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    store_conformance::store_conformance!(MemoryStore::new(store_conformance::DIMENSION));
}
