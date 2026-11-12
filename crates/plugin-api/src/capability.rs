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
