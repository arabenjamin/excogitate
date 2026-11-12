//! Export and import: everything a store holds, as one JSONL file per
//! kind and a manifest, so a deployment can back up, move between stores,
//! or leave. An import goes only into an empty store, and checks the whole
//! export before it writes anything.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Display;
use std::fs::File;
use std::future::Future;
use std::hash::Hash;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use schema::Validator;
use schemars::JsonSchema;
use serde::Serialize;
use serde::de::DeserializeOwned;
use store::{
    Direction, Page, PageRequest, RecordId, RecordQuery, Store, StoreError, TaskQuery, UnitOfWork,
    WriteOp,
};
use uuid::Uuid;

mod lines;

pub use lines::{
    ChunkLine, Counts, DefinitionLine, FORMAT_VERSION, JobLine, LinkLine, Manifest, NoteLine,
    RecordLine, SpendLine, TaskLine, WorkingLine,
};

/// How many of a kind are read from the store at once.
const PAGE: u32 = 500;
/// How many writes an import applies together.
const BATCH: usize = 200;
/// The manifest's file.
const MANIFEST: &str = "manifest.json";

/// Why an export or import couldn't be made.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ArchiveError {
    /// A file couldn't be read or written.
    #[error("{path}: {source}")]
    File {
        /// The file.
        path: String,
        /// What went wrong.
        source: std::io::Error,
    },
    /// A line of an export isn't what its schema says.
    #[error("{file} line {line}: {problem}")]
    Line {
        /// The file.
        file: &'static str,
        /// Its line, from 1.
        line: u64,
        /// What's wrong.
        problem: String,
    },
    /// It can't be done as things are.
    #[error("{0}")]
    Refused(String),
    /// An import failed part way, so the store holds part of the export.
    #[error(
        "the import stopped after {written} writes, so the store holds part of the export; \
         start again from an empty store: {error}"
    )]
    Partial {
        /// How many writes went in.
        written: usize,
        /// Why it stopped.
        error: Box<ArchiveError>,
    },
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn file_error(path: &Path) -> impl FnOnce(std::io::Error) -> ArchiveError + '_ {
    move |source| ArchiveError::File {
        path: path.display().to_string(),
        source,
    }
}

/// One file of an export, being written.
struct Out {
    path: PathBuf,
    file: BufWriter<File>,
    count: u64,
}

impl Out {
    fn create(directory: &Path, name: &str) -> Result<Self, ArchiveError> {
        let path = directory.join(name);
        let file = File::create_new(&path).map_err(file_error(&path))?;
        Ok(Self {
            path,
            file: BufWriter::new(file),
            count: 0,
        })
    }

    fn put(&mut self, line: &impl Serialize) -> Result<(), ArchiveError> {
        serde_json::to_writer(&mut self.file, line)
            .map_err(|error| file_error(&self.path)(error.into()))?;
        self.file.write_all(b"\n").map_err(file_error(&self.path))?;
        self.count += 1;
        Ok(())
    }

    fn finish(self) -> Result<u64, ArchiveError> {
        let Self { path, file, count } = self;
        let file = file
            .into_inner()
            .map_err(|error| file_error(&path)(error.into_error()))?;
        file.sync_all().map_err(file_error(&path))?;
        Ok(count)
    }
}

/// What an export or import held, in a line.
#[must_use]
pub fn summary(counts: &Counts) -> String {
    format!(
        "{} notes, {} chunks, {} tasks, {} working entries, {} jobs, {} definitions, \
         {} records, {} links, {} days of spend",
        counts.notes,
        counts.chunks,
        counts.tasks,
        counts.working,
        counts.jobs,
        counts.definitions,
        counts.records,
        counts.links,
        counts.spend
    )
}

/// Reads every page of a list, in order.
async fn each_page<T, F, Fut>(
    mut fetch: F,
    mut each: impl FnMut(&T) -> Result<(), ArchiveError>,
) -> Result<(), ArchiveError>
where
    F: FnMut(PageRequest) -> Fut,
    Fut: Future<Output = Result<Page<T>, StoreError>>,
{
    let mut offset = 0_u32;
    loop {
        let page = fetch(PageRequest::new(PAGE, offset)?).await?;
        for item in &page.items {
            each(item)?;
        }
        if !page.has_more {
            return Ok(());
        }
        offset = offset
            .checked_add(PAGE)
            .ok_or_else(|| ArchiveError::Refused("there are too many to read".to_owned()))?;
    }
}

/// What an export is made from.
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    /// The store.
    pub store: &'a dyn Store,
    /// Its name, as the manifest gives it: `duckdb` or `neo4j`.
    pub store_name: &'a str,
    /// The runtime making it, with its version.
    pub runtime: &'a str,
    /// Now, which decides what working memory has expired.
    pub now: DateTime<Utc>,
}

/// Makes `directory`, which must not exist or must be empty.
fn fresh(directory: &Path) -> Result<(), ArchiveError> {
    if directory.exists() {
        let mut entries = std::fs::read_dir(directory).map_err(file_error(directory))?;
        if entries.next().is_some() {
            return Err(ArchiveError::Refused(format!(
                "{} isn't empty; export into a new directory",
                directory.display()
            )));
        }
    }
    std::fs::create_dir_all(directory).map_err(file_error(directory))
}

/// Writes the notes and their chunks, each note's chunks in order after
/// it, so an import can read the two files side by side.
async fn export_notes(store: &dyn Store, directory: &Path) -> Result<(u64, u64), ArchiveError> {
    let (mut notes, mut chunks) = (
        Out::create(directory, "notes.jsonl")?,
        Out::create(directory, "chunks.jsonl")?,
    );
    let mut offset = 0_u32;
    loop {
        let page = store.list_notes(PageRequest::new(PAGE, offset)?).await?;
        for note in &page.items {
            notes.put(&NoteLine::from(note))?;
            for chunk in store.list_chunks(note.id).await? {
                chunks.put(&ChunkLine::from(&chunk))?;
            }
        }
        if !page.has_more {
            return Ok((notes.finish()?, chunks.finish()?));
        }
        offset = offset
            .checked_add(PAGE)
            .ok_or_else(|| ArchiveError::Refused("there are too many notes".to_owned()))?;
    }
}

/// Writes everything `source.store` holds into `directory`, which must be
/// new or empty. The manifest goes last, so an export that stopped part way
/// can't be imported.
///
/// # Errors
///
/// [`ArchiveError`] if the directory isn't empty, a file can't be written,
/// or the store fails.
pub async fn export(source: Source<'_>, directory: &Path) -> Result<Manifest, ArchiveError> {
    fresh(directory)?;
    let store = source.store;
    let (notes, chunks) = export_notes(store, directory).await?;
    let mut out = Out::create(directory, "tasks.jsonl")?;
    each_page(
        |page| store.list_tasks(TaskQuery::default(), page),
        |task| out.put(&TaskLine::from(task)),
    )
    .await?;
    let tasks = out.finish()?;
    let mut out = Out::create(directory, "working.jsonl")?;
    each_page(
        |page| store.list_working(source.now, page),
        |entry| out.put(&WorkingLine::from(entry)),
    )
    .await?;
    let working = out.finish()?;
    let mut out = Out::create(directory, "jobs.jsonl")?;
    each_page(
        |page| store.list_jobs(None, page),
        |job| out.put(&JobLine::from(job)),
    )
    .await?;
    let jobs = out.finish()?;
    let mut out = Out::create(directory, "definitions.jsonl")?;
    for kind in lines::KINDS {
        each_page(
            |page| store.list_definitions(kind, page),
            |definition| out.put(&DefinitionLine::from(definition)),
        )
        .await?;
    }
    let definitions = out.finish()?;
    let counts = Counts {
        notes,
        chunks,
        tasks,
        working,
        jobs,
        definitions,
        ..export_records(store, directory).await?
    };
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        exported_at: source.now,
        runtime: source.runtime.to_owned(),
        store: source.store_name.to_owned(),
        embedding_dimension: u32::try_from(store.embedding_dimension()).unwrap_or(u32::MAX),
        counts,
    };
    let path = directory.join(MANIFEST);
    let text =
        serde_json::to_string_pretty(&manifest).map_err(|error| file_error(&path)(error.into()))?;
    std::fs::write(&path, text).map_err(file_error(&path))?;
    Ok(manifest)
}

/// Writes the records, their links and the spend ledger, and counts them.
async fn export_records(store: &dyn Store, directory: &Path) -> Result<Counts, ArchiveError> {
    let mut out = Out::create(directory, "records.jsonl")?;
    each_page(
        |page| store.list_all_records(page),
        |record| out.put(&RecordLine::from(record)),
    )
    .await?;
    let records = out.finish()?;
    let mut out = Out::create(directory, "links.jsonl")?;
    each_page(
        |page| store.list_links(page),
        |link| out.put(&LinkLine::from(link)),
    )
    .await?;
    let links = out.finish()?;
    let mut out = Out::create(directory, "spend.jsonl")?;
    each_page(
        |page| store.list_spend(page),
        |day| out.put(&SpendLine::from(day)),
    )
    .await?;
    Ok(Counts {
        records,
        links,
        spend: out.finish()?,
        ..Counts::default()
    })
}

/// The lines of one file of an export, each checked against its schema.
struct Lines<T> {
    file: &'static str,
    reader: BufReader<File>,
    line: u64,
    validator: Validator,
    kind: PhantomData<T>,
}

impl<T: JsonSchema + DeserializeOwned> Lines<T> {
    fn open(directory: &Path, file: &'static str) -> Result<Self, ArchiveError> {
        let path = directory.join(file);
        let opened = File::open(&path).map_err(file_error(&path))?;
        Ok(Self {
            file,
            reader: BufReader::new(opened),
            line: 0,
            validator: Validator::for_type::<T>()
                .map_err(|error| ArchiveError::Refused(error.to_string()))?,
            kind: PhantomData,
        })
    }

    fn problem(&self, problem: &impl ToString) -> ArchiveError {
        ArchiveError::Line {
            file: self.file,
            line: self.line,
            problem: schema::bounded(&problem.to_string()),
        }
    }
}

impl<T: JsonSchema + DeserializeOwned> Iterator for Lines<T> {
    type Item = Result<T, ArchiveError>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut text = String::new();
        match self.reader.read_line(&mut text) {
            Ok(0) => None,
            Ok(_) => {
                self.line += 1;
                let parsed = serde_json::from_str(&text)
                    .map_err(|error| self.problem(&error))
                    .and_then(|raw| {
                        self.validator
                            .parse::<T>(raw)
                            .map_err(|refusal| self.problem(&refusal))
                    });
                Some(parsed)
            }
            Err(error) => Some(Err(self.problem(&error))),
        }
    }
}

/// Reads the manifest, and refuses a format this build doesn't know.
fn manifest(directory: &Path) -> Result<Manifest, ArchiveError> {
    let path = directory.join(MANIFEST);
    let text = std::fs::read_to_string(&path).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => ArchiveError::Refused(format!(
            "{} has no {MANIFEST}: it isn't an export, or the export didn't finish",
            directory.display()
        )),
        _ => file_error(&path)(source),
    })?;
    let raw = serde_json::from_str(&text).map_err(|error| ArchiveError::Line {
        file: MANIFEST,
        line: 1,
        problem: error.to_string(),
    })?;
    let manifest: Manifest = Validator::for_type::<Manifest>()
        .map_err(|error| ArchiveError::Refused(error.to_string()))?
        .parse(raw)
        .map_err(|refusal| ArchiveError::Line {
            file: MANIFEST,
            line: 1,
            problem: schema::bounded(&refusal.to_string()),
        })?;
    if manifest.format_version != FORMAT_VERSION {
        return Err(ArchiveError::Refused(format!(
            "this export's format is version {}; this build reads version {FORMAT_VERSION}",
            manifest.format_version
        )));
    }
    Ok(manifest)
}

/// Counts a file's lines, checking each, and says if the count isn't the
/// manifest's.
fn count<T: JsonSchema + DeserializeOwned>(
    directory: &Path,
    file: &'static str,
    expected: u64,
    mut each: impl FnMut(T) -> Result<(), ArchiveError>,
) -> Result<(), ArchiveError> {
    let mut seen = 0_u64;
    for line in Lines::<T>::open(directory, file)? {
        each(line?)?;
        seen += 1;
    }
    if seen == expected {
        Ok(())
    } else {
        Err(ArchiveError::Refused(format!(
            "{file} has {seen} lines, and the manifest says {expected}"
        )))
    }
}

/// Refuses a second line with the same key: an import would write over the
/// first and still report both.
fn once<K: Hash + Eq + Display>(
    seen: &mut HashSet<K>,
    file: &str,
    key: K,
) -> Result<(), ArchiveError> {
    if seen.contains(&key) {
        return Err(ArchiveError::Refused(format!(
            "{file} has {key} twice; an export holds each once"
        )));
    }
    seen.insert(key);
    Ok(())
}

/// What the whole export needs of the store it goes into.
#[derive(Debug, Default)]
struct Needs {
    record_types: BTreeSet<String>,
    relations: BTreeSet<String>,
}

/// Checks every line of the export before anything is written: each meets
/// its schema, each file has the manifest's count, chunks follow their
/// notes, and every embedding is `dimension` wide.
fn check_export(
    directory: &Path,
    manifest: &Manifest,
    dimension: usize,
) -> Result<Needs, ArchiveError> {
    let counts = manifest.counts;
    let (mut notes, mut seen) = (Vec::new(), HashSet::new());
    count::<NoteLine>(directory, "notes.jsonl", counts.notes, |note| {
        once(&mut seen, "notes.jsonl", note.id)?;
        notes.push(note.id);
        Ok(())
    })?;
    let mut place = 0_usize;
    let mut seen = HashSet::new();
    count::<ChunkLine>(directory, "chunks.jsonl", counts.chunks, |chunk| {
        once(&mut seen, "chunks.jsonl", chunk.id)?;
        while notes.get(place).is_some_and(|id| *id != chunk.note_id) {
            place += 1;
        }
        if notes.get(place).is_none() {
            return Err(ArchiveError::Refused(format!(
                "chunk {} isn't after its note {} in the notes' order",
                chunk.id, chunk.note_id
            )));
        }
        match &chunk.embedding {
            Some(embedding) if embedding.len() != dimension => Err(ArchiveError::Refused(format!(
                "chunk {} has an embedding {} wide, and this store's embeddings are {dimension} \
                 wide; import into a store set up for the same embedding model",
                chunk.id,
                embedding.len()
            ))),
            _ => Ok(()),
        }
    })?;
    let mut seen = HashSet::new();
    count::<TaskLine>(directory, "tasks.jsonl", counts.tasks, |task| {
        once(&mut seen, "tasks.jsonl", task.id)
    })?;
    let mut seen = HashSet::new();
    count::<WorkingLine>(directory, "working.jsonl", counts.working, |entry| {
        once(
            &mut seen,
            "working.jsonl",
            format!("{}/{}", entry.scope, entry.key),
        )
    })?;
    let mut seen = HashSet::new();
    count::<JobLine>(directory, "jobs.jsonl", counts.jobs, |job| {
        once(&mut seen, "jobs.jsonl", job.id)
    })?;
    let mut seen = HashSet::new();
    count::<DefinitionLine>(
        directory,
        "definitions.jsonl",
        counts.definitions,
        |definition| {
            // A name that couldn't be a file's would be the runtime's way out
            // of its definitions directory.
            if !work::is_definition_name(&definition.name) {
                return Err(ArchiveError::Refused(format!(
                    "definitions.jsonl names a definition {:?}; a name is 1 to 64 lowercase \
                 letters, digits, _ and -",
                    definition.name
                )));
            }
            once(
                &mut seen,
                "definitions.jsonl",
                format!("{:?} {}", definition.kind, definition.name),
            )
        },
    )?;
    let (mut needs, mut seen) = (Needs::default(), HashSet::new());
    count::<RecordLine>(directory, "records.jsonl", counts.records, |record| {
        once(
            &mut seen,
            "records.jsonl",
            format!("{} {}", record.record_type, record.id),
        )?;
        needs.record_types.insert(record.record_type);
        Ok(())
    })?;
    let mut seen = HashSet::new();
    count::<LinkLine>(directory, "links.jsonl", counts.links, |link| {
        once(
            &mut seen,
            "links.jsonl",
            format!("{} {} {}", link.relation, link.from, link.to),
        )?;
        needs.relations.insert(link.relation);
        Ok(())
    })?;
    let mut seen = HashSet::new();
    count::<SpendLine>(directory, "spend.jsonl", counts.spend, |day| {
        once(
            &mut seen,
            "spend.jsonl",
            format!("{} {}", day.provider, day.day),
        )
    })?;
    Ok(needs)
}

/// Refuses a store that holds anything an import would add to.
async fn check_empty(store: &dyn Store) -> Result<(), ArchiveError> {
    let one = PageRequest::new(1, 0)?;
    let mut held = Vec::new();
    if !store.list_notes(one).await?.items.is_empty() {
        held.push("notes");
    }
    if !store
        .list_tasks(TaskQuery::default(), one)
        .await?
        .items
        .is_empty()
    {
        held.push("tasks");
    }
    if !store.list_jobs(None, one).await?.items.is_empty() {
        held.push("jobs");
    }
    if !store.list_all_records(one).await?.items.is_empty() {
        held.push("records");
    }
    if !store.list_links(one).await?.items.is_empty() {
        held.push("links");
    }
    if !store.list_working(Utc::now(), one).await?.items.is_empty() {
        held.push("working memory");
    }
    // Imported spend adds to a day's, and would trip the cap early.
    if !store.list_spend(one).await?.items.is_empty() {
        held.push("spend");
    }
    // Definitions aren't counted: a server loads them from its files at
    // start, and an import replaces each by its name.
    if held.is_empty() {
        Ok(())
    } else {
        Err(ArchiveError::Refused(format!(
            "the store isn't empty: it has {}; import into a new store",
            held.join(", ")
        )))
    }
}

/// Refuses an export holding records or links this build doesn't declare,
/// since the store couldn't check them.
async fn check_declared(store: &dyn Store, needs: &Needs) -> Result<(), ArchiveError> {
    let one = PageRequest::new(1, 0)?;
    let mut missing = Vec::new();
    for record_type in &needs.record_types {
        let probed = store
            .query_records(RecordQuery::new(record_type.clone(), one))
            .await;
        if matches!(probed, Err(StoreError::Invalid(_))) {
            missing.push(format!("record type {record_type}"));
        }
    }
    for relation in &needs.relations {
        let probed = store
            .neighbors(relation, RecordId::generate(), Direction::Outgoing, one)
            .await;
        if matches!(probed, Err(StoreError::Invalid(_))) {
            missing.push(format!("relation {relation}"));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ArchiveError::Refused(format!(
            "no plugin in this build declares {}; import with a build that has the plugins \
             that wrote them",
            missing.join(", ")
        )))
    }
}

/// Writes in batches, and counts what went in.
struct Writer<'a> {
    store: &'a dyn Store,
    pending: Vec<WriteOp>,
    written: usize,
}

impl Writer<'_> {
    async fn push(&mut self, op: WriteOp) -> Result<(), ArchiveError> {
        self.pending.push(op);
        if self.pending.len() >= BATCH {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), ArchiveError> {
        let ops = std::mem::take(&mut self.pending);
        let size = ops.len();
        self.store
            .apply(UnitOfWork { ops })
            .await
            .map_err(|error| ArchiveError::Partial {
                written: self.written,
                error: Box::new(ArchiveError::Store(error)),
            })?;
        self.written += size;
        Ok(())
    }
}

/// Loads an export into `store`, which must be empty. Everything is checked
/// first; only then is anything written.
///
/// # Errors
///
/// [`ArchiveError`] if the export is malformed or doesn't fit the store, if
/// the store isn't empty, or if a write fails, in which case the store
/// holds part of the export.
pub async fn import(store: &dyn Store, directory: &Path) -> Result<Counts, ArchiveError> {
    let manifest = manifest(directory)?;
    let needs = check_export(directory, &manifest, store.embedding_dimension())?;
    check_empty(store).await?;
    check_declared(store, &needs).await?;
    let mut writer = Writer {
        store,
        pending: Vec::new(),
        written: 0,
    };
    // Once anything is written, whatever stops the import leaves part of
    // the export in the store, and says so.
    match write_all(&mut writer, directory).await {
        Ok(()) => Ok(manifest.counts),
        Err(error @ ArchiveError::Partial { .. }) => Err(error),
        Err(error) if writer.written > 0 => Err(ArchiveError::Partial {
            written: writer.written,
            error: Box::new(error),
        }),
        Err(error) => Err(error),
    }
}

/// Writes every file of a checked export, in an order that puts each
/// thing's parts after it.
async fn write_all(writer: &mut Writer<'_>, directory: &Path) -> Result<(), ArchiveError> {
    let store = writer.store;
    for line in Lines::<DefinitionLine>::open(directory, "definitions.jsonl")? {
        writer.push(WriteOp::PutDefinition(line?.into())).await?;
    }
    import_notes(writer, directory).await?;
    for line in Lines::<TaskLine>::open(directory, "tasks.jsonl")? {
        writer.push(WriteOp::PutTask(line?.into())).await?;
    }
    for line in Lines::<WorkingLine>::open(directory, "working.jsonl")? {
        writer.push(WriteOp::PutWorking(line?.into())).await?;
    }
    for line in Lines::<JobLine>::open(directory, "jobs.jsonl")? {
        writer.push(WriteOp::RestoreJob(line?.into())).await?;
    }
    for line in Lines::<RecordLine>::open(directory, "records.jsonl")? {
        writer.push(WriteOp::PutRecord(line?.into())).await?;
    }
    // Links last, so both ends of each are in.
    writer.flush().await?;
    for line in Lines::<LinkLine>::open(directory, "links.jsonl")? {
        let link = line?;
        writer
            .push(WriteOp::Link {
                relation: link.relation,
                from: RecordId::from_uuid(link.from),
                to: RecordId::from_uuid(link.to),
            })
            .await?;
    }
    writer.flush().await?;
    for line in Lines::<SpendLine>::open(directory, "spend.jsonl")? {
        let day = line?;
        store
            .reserve_spend(&day.provider, day.day, day.usd_micros, u64::MAX)
            .await
            .map_err(|error| ArchiveError::Partial {
                written: writer.written,
                error: Box::new(ArchiveError::Store(error)),
            })?;
    }
    Ok(())
}

/// Writes each note with its chunks, reading the two files side by side.
async fn import_notes(writer: &mut Writer<'_>, directory: &Path) -> Result<(), ArchiveError> {
    let mut chunks = Lines::<ChunkLine>::open(directory, "chunks.jsonl")?.peekable();
    for line in Lines::<NoteLine>::open(directory, "notes.jsonl")? {
        let note = line?;
        let id: Uuid = note.id;
        let mut own = Vec::new();
        while let Some(Ok(chunk)) = chunks.peek() {
            if chunk.note_id != id {
                break;
            }
            if let Some(Ok(chunk)) = chunks.next() {
                own.push(chunk.into());
            }
        }
        if let Some(Err(_)) = chunks.peek()
            && let Some(Err(error)) = chunks.next()
        {
            return Err(error);
        }
        let note: store::Note = note.into();
        let note_id = note.id;
        writer.push(WriteOp::CreateNote(note)).await?;
        if !own.is_empty() {
            writer
                .push(WriteOp::ReplaceChunks {
                    note_id,
                    chunks: own,
                })
                .await?;
        }
    }
    Ok(())
}
