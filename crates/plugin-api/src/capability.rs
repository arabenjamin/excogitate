//! What a plugin can ask the runtime to do. Each one is granted by name, and
//! there's deliberately no way to call another tool.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use store::{Direction, Page, PageRequest, Record, RecordId, RecordQuery};

use crate::{PluginCapability, ToolContext};

/// A capability call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CapabilityError {
    /// The plugin wasn't granted this capability.
    #[error("capability {0:?} wasn't granted")]
    NotGranted(PluginCapability),
    /// The request isn't allowed or isn't valid.
    #[error("refused: {0}")]
    Refused(String),
    /// The runtime couldn't do it.
    #[error("failed: {0}")]
    Failed(String),
}

/// The most records or links one batch may carry. A batch also has to fit
/// in one message to the runtime (8 MiB, for a plugin run as a process).
pub const MAX_BATCH: usize = 500;

/// One record to add or replace, in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecordWrite {
    /// Its type.
    pub record_type: String,
    /// The id to replace, or none for a new record.
    pub id: Option<RecordId>,
    /// Its data.
    pub data: Value,
}

impl RecordWrite {
    /// A new record of this type.
    #[must_use]
    pub fn new(record_type: impl Into<String>, data: Value) -> Self {
        Self {
            record_type: record_type.into(),
            id: None,
            data,
        }
    }

    /// The same write, replacing the record with this id.
    #[must_use]
    pub const fn with_id(mut self, id: RecordId) -> Self {
        self.id = Some(id);
        self
    }
}

/// One link to make, in a batch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RecordLink {
    /// The declared relation.
    pub relation: String,
    /// The record it links from.
    pub from: RecordId,
    /// The record it links to.
    pub to: RecordId,
}

impl RecordLink {
    /// A link by `relation` from one record to another.
    #[must_use]
    pub fn new(relation: impl Into<String>, from: RecordId, to: RecordId) -> Self {
        Self {
            relation: relation.into(),
            from,
            to,
        }
    }
}

/// Refuses a batch over [`MAX_BATCH`].
///
/// # Errors
///
/// [`CapabilityError::Refused`] if it's too big.
pub fn check_batch(len: usize) -> Result<(), CapabilityError> {
    if len > MAX_BATCH {
        return Err(CapabilityError::Refused(format!(
            "a batch holds at most {MAX_BATCH}, not {len}"
        )));
    }
    Ok(())
}

/// The plugin's own records, and only its own: every type and relation name
/// must carry the plugin's prefix. The runtime sets each record's provenance.
#[async_trait]
pub trait RecordsCapability: Send + Sync + std::fmt::Debug {
    /// Adds or replaces a record, returning its id.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn put(
        &self,
        record_type: &str,
        id: Option<RecordId>,
        data: Value,
        context: ToolContext,
    ) -> Result<RecordId, CapabilityError>;

    /// A record, if it exists.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn get(
        &self,
        record_type: &str,
        id: RecordId,
        context: ToolContext,
    ) -> Result<Option<Record>, CapabilityError>;

    /// Records meeting typed filters.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn query(
        &self,
        query: RecordQuery,
        context: ToolContext,
    ) -> Result<Page<Record>, CapabilityError>;

    /// Removes a record and its links.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn delete(
        &self,
        record_type: &str,
        id: RecordId,
        context: ToolContext,
    ) -> Result<(), CapabilityError>;

    /// Links two records by a declared relation.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn link(
        &self,
        relation: &str,
        from: RecordId,
        to: RecordId,
        context: ToolContext,
    ) -> Result<(), CapabilityError>;

    /// The records a relation links to or from `id`.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn neighbors(
        &self,
        relation: &str,
        id: RecordId,
        direction: Direction,
        page: PageRequest,
        context: ToolContext,
    ) -> Result<Page<Record>, CapabilityError>;

    /// Adds or replaces up to [`MAX_BATCH`] records, returning their ids in
    /// order. The runtime writes a batch as one unit: all of it, or none if
    /// any record is refused. This default, for a plugin's own test
    /// doubles, puts them one at a time.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if the batch, or any record in it, is refused or
    /// fails.
    async fn put_many(
        &self,
        records: Vec<RecordWrite>,
        context: ToolContext,
    ) -> Result<Vec<RecordId>, CapabilityError> {
        check_batch(records.len())?;
        let mut ids = Vec::with_capacity(records.len());
        for write in records {
            ids.push(
                self.put(&write.record_type, write.id, write.data, context)
                    .await?,
            );
        }
        Ok(ids)
    }

    /// Makes up to [`MAX_BATCH`] links. The runtime makes a batch as one
    /// unit: all of it, or none if any link is refused. This default, for
    /// a plugin's own test doubles, links them one at a time.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if the batch, or any link in it, is refused or
    /// fails.
    async fn link_many(
        &self,
        links: Vec<RecordLink>,
        context: ToolContext,
    ) -> Result<(), CapabilityError> {
        check_batch(links.len())?;
        for link in links {
            self.link(&link.relation, link.from, link.to, context)
                .await?;
        }
        Ok(())
    }
}

/// An HTTP request, which the runtime makes under its own network rules.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpRequest {
    /// The method.
    pub method: String,
    /// The URL.
    pub url: String,
    /// Headers. Credentials are attached by host configuration, never here.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Option<String>,
}

impl HttpRequest {
    /// A request with no headers or body.
    #[must_use]
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// A `GET` of `url`.
    #[must_use]
    pub fn get(url: impl Into<String>) -> Self {
        Self::new("GET", url)
    }

    /// The same request with one more header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The same request with a body.
    #[must_use]
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }
}

/// An HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The body, cut to the host's limit. Empty when there's none to give.
    pub body: String,
    /// Whether the body was cut.
    pub truncated: bool,
    /// Why the body is missing, such as a type that isn't text.
    pub note: Option<String>,
}

impl HttpResponse {
    /// A whole response.
    #[must_use]
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            truncated: false,
            note: None,
        }
    }

    /// The same response, marked as cut or not.
    #[must_use]
    pub const fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// The same response, saying why its body is missing.
    #[must_use]
    pub fn with_note(mut self, note: Option<String>) -> Self {
        self.note = note;
        self
    }
}

/// HTTP through the runtime, under the same network rules as the
/// `http_request` tool. What comes back is untrusted, and so is everything
/// a plugin holding this capability returns.
#[async_trait]
pub trait HttpCapability: Send + Sync + std::fmt::Debug {
    /// Makes a request. Private addresses are refused by the host.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn request(
        &self,
        request: HttpRequest,
        context: ToolContext,
    ) -> Result<HttpResponse, CapabilityError>;
}

/// A model's answer, and what it cost.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ModelReply {
    /// The text it returned.
    pub text: String,
    /// The model that answered.
    pub model: String,
    /// What the call cost, in millionths of a dollar. The host charges it to
    /// the run whatever the plugin reports.
    pub cost_usd_micros: u64,
}

impl ModelReply {
    /// An answer from `model`, and what it cost.
    #[must_use]
    pub fn new(text: impl Into<String>, model: impl Into<String>, cost_usd_micros: u64) -> Self {
        Self {
            text: text.into(),
            model: model.into(),
            cost_usd_micros,
        }
    }
}

/// One-shot model calls, routed and charged by the runtime.
#[async_trait]
pub trait ModelCapability: Send + Sync + std::fmt::Debug {
    /// Asks a model. The runtime picks the model, under the run's privacy
    /// class and within what's left of its budget.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused or fails.
    async fn complete(
        &self,
        prompt: &str,
        context: ToolContext,
    ) -> Result<ModelReply, CapabilityError>;
}

/// The longest title a stored note may have.
pub const MAX_NOTE_TITLE: usize = 256;
/// The longest text a stored note may have.
pub const MAX_NOTE_TEXT: usize = 100_000;
/// The most tags a stored note may have.
pub const MAX_NOTE_TAGS: usize = 16;
/// The longest tag.
pub const MAX_NOTE_TAG: usize = 64;
/// The longest source URL.
pub const MAX_NOTE_URL: usize = 2048;

/// A note for long-term memory, as a plugin offers it. Who may see it and
/// whether it's trusted are the runtime's to say, never the plugin's.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NoteToStore {
    /// A short title.
    pub title: String,
    /// The text.
    pub text: String,
    /// Tags.
    pub tags: Vec<String>,
    /// The URL it came from.
    pub source_url: Option<String>,
}

impl NoteToStore {
    /// A note with no tags or source.
    #[must_use]
    pub fn new(title: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            text: text.into(),
            tags: Vec::new(),
            source_url: None,
        }
    }

    /// The same note, with these tags.
    #[must_use]
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// The same note, from this URL.
    #[must_use]
    pub fn with_source_url(mut self, url: impl Into<String>) -> Self {
        self.source_url = Some(url.into());
        self
    }

    /// Refuses a note past the limits, or with no text.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::Refused`], saying which limit.
    pub fn check(&self) -> Result<(), CapabilityError> {
        let refuse = |why: String| Err(CapabilityError::Refused(why));
        let chars = |text: &str| text.chars().count();
        if self.text.trim().is_empty() {
            return refuse("a note needs some text".to_owned());
        }
        if chars(&self.text) > MAX_NOTE_TEXT {
            return refuse(format!(
                "a note's text is at most {MAX_NOTE_TEXT} characters"
            ));
        }
        if chars(&self.title) > MAX_NOTE_TITLE {
            return refuse(format!(
                "a note's title is at most {MAX_NOTE_TITLE} characters"
            ));
        }
        if self.tags.len() > MAX_NOTE_TAGS || self.tags.iter().any(|tag| chars(tag) > MAX_NOTE_TAG)
        {
            return refuse(format!(
                "a note has at most {MAX_NOTE_TAGS} tags, each at most {MAX_NOTE_TAG} characters"
            ));
        }
        if self
            .source_url
            .as_deref()
            .is_some_and(|url| chars(url) > MAX_NOTE_URL)
        {
            return refuse(format!("a source URL is at most {MAX_NOTE_URL} characters"));
        }
        Ok(())
    }
}

/// What storing a note did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct NoteStored {
    /// The note's id.
    pub id: uuid::Uuid,
    /// False when the same text, in the same privacy class, was already
    /// kept: this is that note.
    pub created: bool,
    /// How many chunks it has.
    pub chunks: u32,
    /// How many have no vector, and so are found only by their words.
    pub unembedded_chunks: u32,
    /// What embedding it cost, in millionths of a dollar. It's charged to
    /// the run.
    pub cost_usd_micros: u64,
}

impl NoteStored {
    /// What a store did.
    #[must_use]
    pub const fn new(id: uuid::Uuid, created: bool, chunks: u32, unembedded_chunks: u32) -> Self {
        Self {
            id,
            created,
            chunks,
            unembedded_chunks,
            cost_usd_micros: 0,
        }
    }

    /// The same, with what it cost.
    #[must_use]
    pub const fn with_cost(mut self, cost_usd_micros: u64) -> Self {
        self.cost_usd_micros = cost_usd_micros;
        self
    }
}

/// Writing into the runtime's long-term memory, so its search finds what a
/// plugin brought in. Each note is kept under the calling run's privacy
/// class, marked untrusted, and stored by `plugin:<name>`.
#[async_trait]
pub trait MemoryCapability: Send + Sync + std::fmt::Debug {
    /// Stores a note. Its embedding is paid from what the run has left.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it's refused, as past a limit, or fails.
    async fn store(
        &self,
        note: NoteToStore,
        context: ToolContext,
    ) -> Result<NoteStored, CapabilityError>;
}

/// The most jobs one plugin may have waiting or running at once. Past it a
/// start is refused, so a workflow that calls the plugin again can't
/// multiply without end.
pub const MAX_OUTSTANDING_JOBS: usize = 100;
/// The longest idempotency key.
pub const MAX_IDEMPOTENCY_KEY: usize = 128;

/// A workflow run for a plugin to start. It runs as its own job, held to its
/// workflow's budget, and is retried never, since a retry would repeat what
/// its earlier steps did.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunWorkflow {
    /// The workflow.
    pub workflow: String,
    /// Its inputs, checked against the workflow's inputs schema.
    pub inputs: Value,
    /// Higher runs first, from -1000 to 1000.
    pub priority: i32,
    /// The same key for the same workflow starts it once: a second start
    /// returns the first run's job, for as long as that job is kept.
    pub idempotency_key: Option<String>,
}

impl RunWorkflow {
    /// A run of `workflow` with `inputs`, at priority 0.
    #[must_use]
    pub fn new(workflow: impl Into<String>, inputs: Value) -> Self {
        Self {
            workflow: workflow.into(),
            inputs,
            priority: 0,
            idempotency_key: None,
        }
    }

    /// The same run, at this priority.
    #[must_use]
    pub const fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// The same run, started once per key.
    #[must_use]
    pub fn with_idempotency_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }
}

/// A run that was started, or found already started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct JobStarted {
    /// Its job.
    pub job_id: uuid::Uuid,
    /// True when its idempotency key had started it already.
    pub already_existed: bool,
}

impl JobStarted {
    /// What a start did.
    #[must_use]
    pub const fn new(job_id: uuid::Uuid, already_existed: bool) -> Self {
        Self {
            job_id,
            already_existed,
        }
    }
}

/// Where a job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobPhase {
    /// Waiting to run.
    Queued,
    /// Running.
    Running,
    /// Finished.
    Succeeded,
    /// Failed for good.
    Dead,
}

/// A job a plugin started, as it stands.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct JobState {
    /// The job.
    pub job_id: uuid::Uuid,
    /// Where it stands.
    pub phase: JobPhase,
    /// Why its last attempt failed, if it did.
    pub last_error: Option<String>,
    /// What it produced, once it's finished.
    pub result: Option<Value>,
}

impl JobState {
    /// A job's state.
    #[must_use]
    pub const fn new(job_id: uuid::Uuid, phase: JobPhase) -> Self {
        Self {
            job_id,
            phase,
            last_error: None,
            result: None,
        }
    }

    /// The same, with why it failed and what it produced.
    #[must_use]
    pub fn with_outcome(mut self, last_error: Option<String>, result: Option<Value>) -> Self {
        self.last_error = last_error;
        self.result = result;
        self
    }
}

/// Starting work: workflow runs, each its own job. A plugin sees only the
/// jobs it started.
#[async_trait]
pub trait JobsCapability: Send + Sync + std::fmt::Debug {
    /// Starts a workflow run, once the workflow exists and the inputs fit.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::Refused`] if there's no such workflow, the inputs
    /// don't fit, or the plugin has [`MAX_OUTSTANDING_JOBS`] already.
    async fn run_workflow(
        &self,
        run: RunWorkflow,
        context: ToolContext,
    ) -> Result<JobStarted, CapabilityError>;

    /// A job this plugin started; none for any other, or one not kept.
    ///
    /// # Errors
    ///
    /// [`CapabilityError`] if it fails.
    async fn job(
        &self,
        job_id: uuid::Uuid,
        context: ToolContext,
    ) -> Result<Option<JobState>, CapabilityError>;
}

/// The capabilities handed to a plugin: those it declared and was granted.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Capabilities {
    /// Its own records, if granted.
    pub records: Option<Arc<dyn RecordsCapability>>,
    /// HTTP, if granted.
    pub http: Option<Arc<dyn HttpCapability>>,
    /// Model calls, if granted.
    pub model: Option<Arc<dyn ModelCapability>>,
    /// Long-term memory, if granted.
    pub memory: Option<Arc<dyn MemoryCapability>>,
    /// Starting work, if granted.
    pub jobs: Option<Arc<dyn JobsCapability>>,
}

impl Capabilities {
    /// The same set, with the records capability.
    #[must_use]
    pub fn with_records(mut self, records: Arc<dyn RecordsCapability>) -> Self {
        self.records = Some(records);
        self
    }

    /// The same set, with the HTTP capability.
    #[must_use]
    pub fn with_http(mut self, http: Arc<dyn HttpCapability>) -> Self {
        self.http = Some(http);
        self
    }

    /// The same set, with the model capability.
    #[must_use]
    pub fn with_model(mut self, model: Arc<dyn ModelCapability>) -> Self {
        self.model = Some(model);
        self
    }

    /// The same set, with the memory capability.
    #[must_use]
    pub fn with_memory(mut self, memory: Arc<dyn MemoryCapability>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// The same set, with the jobs capability.
    #[must_use]
    pub fn with_jobs(mut self, jobs: Arc<dyn JobsCapability>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    /// The jobs capability.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::NotGranted`] if the plugin doesn't have it.
    pub fn jobs(&self) -> Result<Arc<dyn JobsCapability>, CapabilityError> {
        self.jobs
            .clone()
            .ok_or(CapabilityError::NotGranted(PluginCapability::Jobs))
    }

    /// The memory capability.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::NotGranted`] if the plugin doesn't have it.
    pub fn memory(&self) -> Result<Arc<dyn MemoryCapability>, CapabilityError> {
        self.memory
            .clone()
            .ok_or(CapabilityError::NotGranted(PluginCapability::Memory))
    }

    /// The records capability.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::NotGranted`] if the plugin doesn't have it.
    pub fn records(&self) -> Result<Arc<dyn RecordsCapability>, CapabilityError> {
        self.records
            .clone()
            .ok_or(CapabilityError::NotGranted(PluginCapability::Records))
    }

    /// The HTTP capability.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::NotGranted`] if the plugin doesn't have it.
    pub fn http(&self) -> Result<Arc<dyn HttpCapability>, CapabilityError> {
        self.http
            .clone()
            .ok_or(CapabilityError::NotGranted(PluginCapability::Http))
    }

    /// The model capability.
    ///
    /// # Errors
    ///
    /// [`CapabilityError::NotGranted`] if the plugin doesn't have it.
    pub fn model(&self) -> Result<Arc<dyn ModelCapability>, CapabilityError> {
        self.model
            .clone()
            .ok_or(CapabilityError::NotGranted(PluginCapability::Model))
    }
}
